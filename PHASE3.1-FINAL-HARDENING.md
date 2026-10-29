# Phase 3.1 — final hardening

Validation date: 2026-10-29, Asia/Manila. Internal engineering review; no external audit, tag, package publication or release. PR #11 was merged by the user while long-running checks continued; this follow-up records their completed results.

## Source baseline

The starting worktree was clean, on `dev` at
`725ad4e` (merge of PR #10). Phase 1 PR #8 and Phase 2 PR #9 were already
included; Phase 3 was committed at `de7aeb8` and merged. The historical Phase 3
report's uncommitted-state description was no longer current. All existing
ignored reports were read and preserved. Work began on
`perf/phase3-final-hardening` from that merge. The two requested milestones form one combined review stack. The user later
authorized committing all current Git changes, pushing and opening a PR into
dev without trailers; no pre-existing work was discarded.

The Phase 2 comparison is `eed23e1`, in a separate detached worktree. Its only
adaptation is the Phase 3 hotpath benchmark source and a benchmark target in
Cargo.toml; no baseline production Rust source was changed. The first campaign was invalidated after a load-generator defect and a runner
failure were found. Replacement binaries contain the exact final combined
hardening/production source (0.4.0 candidate). Their build-input identity is
`5770f32c813e45fd51452031ea64557ee4baa25d3c6c8adabc9aa6221a0567ea`;
the baseline's adapted build-input identity is
`f304269e07914f42856f4849437796d715b563bc9a357d03121be851fda1ee1a`.
Final manifests were recorded from clean commit `325aa63`; later shell, Python, workflow and documentation corrections do not change this Rust/Cargo/embedded-asset digest or the measured binaries. Invocation metadata can name the live ancestor checkout commit; executable sidecars, binary hashes and frozen harness hashes identify the actual inputs. Sidecars record all Rust/Cargo/toolchain inputs and embedded `src/` assets, lockfile hashes, rustc identity and binary SHA-256. The implementation branch was `feat/phase4-production-readiness`, reviewed in [PR #11](https://github.com/akosidencio/tanod/pull/11) and merged into `dev` by the user. This follow-up records validation on `docs/phase4-validation-results`; no release was performed.

## Cache concurrency review and linearization

The second completed-map lookup is sufficient for the ordinary stored
completion transition under the actual locks. A lookup first checks `cached`,
then `temp`. Completion inserts into `cached` before removing its exact writer
ID from `temp`. A temporary-map read that sees the removal synchronizes after
that insertion. Its second completed-map read therefore finds the entry unless
there was an actual eviction/replacement/removal. A missing temporary entry
alone was not enough: the first cache read could precede admission.

| Operation | Linearization / synchronization |
|---|---|
| Completed hit | Clone metadata/body while holding `cached.read`, inside `lifecycle.read`. |
| New follower | Check eligibility and subscribe under the same temporary-map guard, inside `lifecycle.read`. |
| Completion/admission | Insert completed entry under accounting/cache locks, while holding `lifecycle.read`; remove the exact temporary writer afterward. |
| Purge | Release `lifecycle.write` after invalidating matching registered fills and removing completed entries/index records. Completion cannot interleave inside that transition. |
| Namespace retirement | Change active identity and invalidate/reclaim under `lifecycle.write`, before policy publication. Late registration compares its typed namespace under `lifecycle.read`. |
| Eviction | Remove matching entry revision under accounting/cache locks. Attached readers retain body references; genuine eviction permits a later miss. |
| Cancellation | Dropping an unfinished writer removes its exact temporary ID and releases its reservation once. A closed incomplete progress channel makes waiting readers fail rather than hang. |
| Memory abandonment | Reservation feasibility checks pinned bytes before eviction; abandoned fills use bounded headroom, cannot persist, and are removed/released on finish/drop. |

Lock order remains lifecycle → accounting → cached → order → tag index →
TinyUFO, omitting unused locks. Completion releases the temporary-map guard
before accounting/admission locks. No blocking lock crosses an await. Once
polled, `finish` has no suspension point: task cancellation can drop the
unpolled future or take effect after the synchronous transition, not interrupt
an arbitrary instruction inside it. Handoff expiry removes the exact temporary
ID before releasing its reservation; it does not share release ownership with
an admitted/discarded writer.

One additional invariant gap was fixed. A request pinned to an obsolete policy
could register its fill after retirement. Completion already refused to retain
it, and namespace-encoded keys isolated it from current requests, but new
old-namespace lookups could still attach while it streamed. Registration now
marks that fill ineligible immediately. Its leader still receives the reader
reserved at registration; a second exact-tag lookup cannot create a new
subscription. This is a strengthened retirement invariant, not evidence of a
cross-namespace response disclosure.

Existing followers keep progressive bytes across purge, retirement and eviction.
Purge does not revoke an already delivered response, and same-namespace origin
work not yet registered at purge time remains outside its transaction. Logical
cache accounting still excludes detached readers, allocator slack and indexes.

## Deterministic and model validation

`store_phase31_tests.rs` adds eight tests. Per-store one-shot hooks, compiled
out of production, force completion between the lookup's two map reads, purge
and retirement before completion or after admission, eviction between admission
and temporary removal, attachment on both sides of those transitions, and
unpolled-finish cancellation. Eight concurrent writers complete 2,400 distinct
fills per policy, checking whole follower bodies and exact final accounting.

The Loom model explores completion, lookup and purge using separate lifecycle
and map locks. It asserts normal completion visibility, post-purge exclusion and
no stale resurrection. It abstracts eligibility/presence, not actual body
storage, metadata, eviction or TinyUFO internals; it complements deterministic
real-store tests rather than proving the entire implementation correct.

Initial targeted result: **8 passed**, including Loom and both policies, in
2.67 seconds. The pre-change full suite passed with local loopback access:
580 library, 7 binary, 6 spike and 3 origin tests (plus the benchmark smoke).
The first restricted run's 17 socket/process-fixture failures were permission
errors, not hidden test passes. Final validation results are recorded below
and in the Phase 4 report.

## Measurement defects corrected

The timed load generator discarded a connection's entire accumulated tally if
its last exchange exceeded the collector's 200 ms allowance. It also required
both request start and completion inside the measured window, biasing long
responses. Each final exchange is now bounded by the window deadline; every
connection returns its accumulated tally; boundary-censored work is counted
separately from transport errors; completions in the measured window are kept
regardless of whether they began during warmup. A real loopback regression
forces one complete response followed by a hanging exchange, with and without
crossing warmup. A task failure now fails the run instead of disappearing.

The allocation miss driver also used timed requests, allowing a censored final
exchange to allocate without entering the completed-request denominator. It now
drives exactly N unique requests on one persistent connection and verifies each
response before recording N. Performance runs refuse transport errors and
unexpected HTTP failures while retaining intentional overload/pressure outcomes.

The initial runner failed after its shell file was edited during execution.
The replacement campaign freezes the complete harness as well as binaries,
records its file hashes, and leaves that snapshot untouched. The invalidated campaign is retained locally and supplies no final headline or regression claim.

A subsequent corrected campaign exposed an independent origin-fixture defect at 512 cold connections: the Tokio/Mio default listen backlog of 128 overflowed. Kernel counters recorded 76 listen overflows/drops, the receive queue reached 129, and Tanod logged upstream connection timeouts at the existing 500 ms deadline. Origin CPU peaked at 0.412 cores. This was a connection-establishment bottleneck in the fixture, not a proven TinyUFO admission failure. The fixture now explicitly listens with backlog 4096. Eight repeated bursts then passed with no listen drops or HTTP failures; maximum observed queue was 40 and origin CPU 0.424 cores. The production connect timeout is unchanged. Rebuilding confirmed identical Tanod and load-generator bytes. The final full campaign starts fresh with this corrected fixture for both policies and all trials; earlier partial results are not spliced into it.

## Performance and worker investigation

The final A/B matrix completed all three pairs under both policies without measured transport or unexpected HTTP errors. At 1 KiB/c32, CLOCK median throughput was 77,214.7 RPS (58,677.3–78,163.1), versus 27,205.9 (26,408.0–28,181.4) for Phase 2. Median p99 was 885 µs (745–1,469), versus 1,347 (1,283–1,393). At c8 it was 177 µs (173–464), versus 368 (358–374): the third candidate trial was slower than every baseline trial despite the improved median. CPU per completed response at c32 rose from 32.4 to 33.6 µs. These results do not establish that scheduler variance is solved.

TinyUFO c32 median p99 was 2,928 µs (692–3,276), versus 2,071 (1,754–4,121); the wide overlapping ranges do not isolate a new runtime regression. Background browser/editor activity was observed, CPU frequencies were not fixed, and this investigation does not fully rule out generator saturation or separate scheduling from lock contention. No quieter machine was available.

A material large-body tradeoff remains: CLOCK 4 MiB/c512 median p99 increased from 933,589 to 1,325,301 µs, with non-overlapping observed ranges (927,350–947,671 versus 1,225,876–1,437,011). Throughput increased from 810.2 to 1,099.0 RPS, while CPU per completed response increased from 951.9 to 1,871.1 µs. At c32, peak-window RSS fell from 141,816 to 115,384 KiB, but CPU per response rose from 951.6 to 1,370.5 µs. Reduced-worker measurements below guide deployments that value CPU efficiency or large-body tails over peak small-body throughput. The default is not changed based solely on this workload.

CPU per completed response includes work on requests still in flight at window boundaries. Short windows and recorded censoring constrain interpretation, especially for multi-megabyte bodies at c512. The full trial CSV retains p50/p95/p99, CPU, context switches, RSS and censoring rather than only favorable samples. Short local trials are not an isolated-host
statistical guarantee. No claim is made that the double-lookup correction has
negligible cost independently of the other Phase 3 changes.

The worker default still calls `std::thread::available_parallelism()` and sets
Pingora's proxy-service thread count only. Admin/metrics retain their existing
single workers. Explicit values 1–1024 are accepted, zero is rejected, and a
changed worker count requires restart. No custom CPU detector was added.

Linux affinity startup probes passed with 1, 2 and 4 CPUs, an explicit four-worker
override on one CPU, and two/four replicas sharing one CPU. Each idle one-second
window recorded 0 scheduler ticks and 0 context switches (timer-resolution
observation, not proof of zero CPU). RSS was roughly 10–11 MiB per cache-disabled
process. Quota/container results belong to the Phase 4 container evidence.
cgroup v1 cannot be emulated honestly on a v2-only host.

The complete worker table is [retained with the evidence](validation/phase31/summary/workers.md). For 1 KiB/c32, one/two/three workers delivered median 26,664/43,372/77,215 RPS with 32.7/36.6/33.6 CPU µs per completion. Their p99 medians were 1,649/2,376/885 µs. A bounded two-worker setting was not a universal latency win.

For 4 MiB/c512, one worker used median 844.3 CPU µs/completion (835.2–893.8), versus 1,871.1 (1,741.4–1,876.4) for three. Median throughput was 899.2 versus 1,099.0 RPS; p99 was 1,068,796 versus 1,325,301 µs, but the one-worker range extended to 1,586,329 µs. Context switches/completion were approximately 0.25 versus 1.34. An explicit single worker is a supported CPU-efficiency choice, not a promise of consistently lower worst-case latency. Tune on the application's body sizes, latency objective and CPU allocation; keep the affinity/quota-derived default for general dedicated proxy capacity rather than lowering it from one workload.

The one/two-worker sweeps ran sequentially, with three retained trials and process reuse within each scenario; the three-worker comparison comes from the earlier interleaved A/B runs. This is not a same-round randomized worker experiment. Allocator retention makes the sweep's RSS particularly sensitive to order. Idle/affinity/replica startup probes and quota tests provide separate resource evidence.

## TinyUFO and rejected changes

TinyPolicy::evict fills unused policy weight with temporary pressure tokens
because the dependency's public API chooses victims on insertion. In-progress
fills occupy exact Tanod reservations but are not completed policy entries, so
large pinned fractions leave a policy-weight gap. Up to 257 pressure chunks can
be needed, followed by lazy-record compaction, under the accounting/cache/policy
locks. This explains why source inspection predicts higher fill contention;
the final measurements preserve that limitation. At 512 tasks the fill-only microbenchmark was 7,140 fills/s (6,920–7,160), versus 7,065 (6,640–7,640) for Phase 2. The 10%-fill mix was 39,880 ops/s (23,320–55,150), versus 40,940 (38,830–46,030). Neither shows a repeatable TinyUFO store optimization. Full-proxy 512-connection fills were faster (median 9,161 versus 4,433 RPS) but remained below candidate CLOCK's 13,023; these are different workloads and worker configurations, not interchangeable fill rates.

A user-CPU `perf` profile retained 51,160 samples with none lost. Flat self samples included TinyUFO FIFO admission (9.37%), estimator construction (3.84%), parking_lot's slow mutex path (1.41%), `reserve_fill` (0.84%) and policy compaction (0.82%), alongside substantial header compression/decoding and allocation work. These are across the selected TinyUFO fill scenarios and setup, not percentages uniquely attributable to pressure-token eviction. No call stacks or off-CPU wait-time attribution were collected. Source review plus this profile supports further policy/compaction investigation, not a safe semantics-preserving optimization in this change.

Pingora 0.9 supports a process configuration `work_stealing` switch, but Tanod does not expose it in its operator schema. Its experimental alternative timer additionally needs `tokio_unstable`. Neither is an available runtime setting for these final-source binaries. The measured supported alternatives are explicit one/two/default proxy-worker counts. No scheduler claim is inferred from source inspection alone.

No eviction-policy fork, batching of victim selection, sharding, global exclusive
completion lock, default-worker reduction or scheduler change was introduced.
Those changes would need repeatable measurements and proof that admission,
revision, memory and fairness semantics remain intact. Existing CLOCK/TinyUFO
behavior and explicit worker overrides are retained.

## Microbenchmarks and allocations

Three paired hotpath trials reproduced the CLOCK refill improvement at 100,000 entries: median 10,260 ns/op (10,210–12,130), versus 3,125,000 (2,299,000–3,390,000). At 1,000 routes, indexed last-section resolution was 522.1 ns/op (516.7–546.4), versus 243,300 (131,800–503,800). Immediate first-route matches cost more: 490.0 versus 94.6 ns/op. Key construction was 2,392 versus 2,341 ns/op, within the observed ranges. The microbenchmarks use up to six unpinned runtime workers as recorded per row; full-proxy affinity settings do not apply to them.

DHAT drives 200 requests/type on one persistent client connection, subtracting startup/shutdown counts. SSR hits used 120.1 allocations and 46,205 bytes/completed request, versus 140.4 and 78,737. Unique SSR misses used 274.6 allocations and 242,650 bytes, versus 280.6 and 240,537: fewer allocations did not mean fewer allocated bytes. Private requests used 147.5 versus 146.6 allocations. These are single fixed-count profiles, not timing trials or a claim that every path allocates less. Raw profiles retain all eight request types and exact denominators; hit types also perform their documented priming request.

## Reproduction and evidence

The paired benchmark build command was `cargo build --workspace --release --locked --bins --bench hotpath` on both sides. Cargo unifies dev features for that target selection, including Tokio `test-util`; the application's printed feature list only reports its own TLS switch. The ordinary workspace release rebuild has a different binary SHA (`cbb746235ecb80b2616fb5632a97c4f4f2eb9ab412d5b5418ff2c520bff66cb5`) from the benchmark build even with identical Rust inputs. Three additional interleaved hit/size/fill comparisons per policy check this graph difference at c8/c32/c512. The final capacity demonstration uses the ordinary-release binary. These measurements are reported separately, rather than claiming benchmark and packaged binary identity.

Build with the pinned Rust 1.98.0 toolchain and locked dependencies; build the
Phase 2 worktree with only the benchmark adaptation described above. Freeze
binaries, record manifests, and do not compile during the campaign:

```sh
python3 scripts/build-manifest.py /absolute/baseline
python3 scripts/build-manifest.py /absolute/candidate
PERF_DURATION=2 PERF_WARMUP=1 PERF_ROUNDS=3 \
LOADGEN_BIN=/absolute/loadgen ORIGIN_BIN=/absolute/slow-origin \
  bash bench/phase31.sh /absolute/baseline /absolute/candidate
```

The recorded campaign uses Intel Core i5-9400F (six cores), about 16 GiB RAM,
Fedora 44, Rust 1.98.0, thin LTO and one codegen unit. Tanod uses CPUs 0–2,
the two-thread load generator 3–4, origin CPU 5. The `powersave` CPU governor and per-run load averages are retained. This is a desktop host with background activity; no quieter host was available. A brief cached image selection happened during the first baseline sweep, without compilation; the full distributions remain visible. Full-system windows are two seconds (body-size
windows one second), after warmup, across three interleaved trials; they are
sampling evidence, not release-grade isolated-host precision. Connections:
1, 8, 32, 100, 128, 512. Bodies: 1 KiB, 16 KiB, 64 KiB, 1 MiB, 4 MiB.
CLOCK and TinyUFO use identical budgets/workloads. Worker alternatives explicitly
set one and two workers against the affinity-derived default of three.

The final full-system campaign produced 840 measured rows, including the original capacity demonstration. A subsequent counter review found that outstanding requests could carry between demo cases: resetting counters does not cancel an origin's old queue. The corrected demonstration starts fresh origin/proxy processes per case and is run separately after stability/build loads finish. Its new data supersedes only the old demo counters; A/B, worker, hotpath and allocation results are unaffected. Both raw versions are retained with explicit scope.

The [comparison table](validation/phase31/summary/comparison.md), [worker table](validation/phase31/summary/workers.md), [ordinary-release comparison](validation/phase31/summary/production-graph.md) and [current trial CSV](validation/phase31/summary/trials-current.csv) retain all distributions. Current data has 1,092 rows: 588 Phase 2/candidate comparisons, 216 worker samples, 252 benchmark/ordinary-release graph comparisons and 36 isolated demonstration samples. The all-trial CSV additionally retains 36 superseded demo rows whose origin peaks included prior outstanding work.

The ordinary-release comparison is materially different from assuming binary equivalence. CLOCK c32 hit throughput was median 50,942 RPS (38,919–63,226), versus 60,903 (59,671–73,587) in the simultaneously paired benchmark build. Its p99 median was 1,708 µs (1,587–3,014), versus 1,605 (952–1,712). The exact build graph matters, and these noisy short samples do not establish a negligible effect. Do not advertise the earlier benchmark-build headline as a packaged-binary measurement.

## Completion assessment

Cache completion, purge, retirement, attachment, cancellation and accounting checks passed under both policies, including forced schedules and the scoped Loom model. Final-source performance and worker investigations are recorded with their build-graph and sampling limits; remaining TinyUFO contention and large-body CPU/tail costs are retained rather than hidden by a speculative optimization.

**The production milestone is NOT READY.** Both mandatory hour-long release soaks completed without unexpected client responses and with stable RSS, but their origin peak reached 10 against a limit of 8. A controlled diagnostic confirms that uncached serving and cache-pressure fallback can release a proxy permit after disconnect while non-cooperative backend work continues; ordinary cached serving retained its permit in that diagnostic. This establishes a lifecycle boundary, not a complete causal attribution of every soak peak. The unchanged strict gate remains red until the origin-work/cancellation contract is resolved and both hour-long runs pass. See the [Phase 4 report](PHASE4-PRODUCTION-READINESS.md).
