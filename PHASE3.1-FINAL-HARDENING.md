# Phase 3.1 — final hardening

Validation date: 2026-10-29, Asia/Manila. Internal engineering review; no external
audit, merge, tag, package publication or release.

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
`31989fb9b80e57723b0b99fd7c3fd65e520e810df9e7edb85780bb9b07e8ad9d`;
the baseline's adapted build-input identity is
`f304269e07914f42856f4849437796d715b563bc9a357d03121be851fda1ee1a`.
Sidecars record the full file hashes, lockfile hashes, rustc identity and binary
SHA-256; these distinguish dirty-source builds from a Git commit claim.

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
records its file hashes, and leaves that snapshot untouched. The invalidated
campaign is retained locally and supplies no final headline or regression claim.

## Performance and worker investigation

Measurements and final assessment are populated from the completed campaign
in the evidence section below. Short local trials are not an isolated-host
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

## TinyUFO and rejected changes

TinyPolicy::evict fills unused policy weight with temporary pressure tokens
because the dependency's public API chooses victims on insertion. In-progress
fills occupy exact Tanod reservations but are not completed policy entries, so
large pinned fractions leave a policy-weight gap. Up to 257 pressure chunks can
be needed, followed by lazy-record compaction, under the accounting/cache/policy
locks. This explains why source inspection predicts higher fill contention;
profiling and repeated measurements are needed before attributing a percentage.

No eviction-policy fork, batching of victim selection, sharding, global exclusive
completion lock, default-worker reduction or scheduler change was introduced.
Those changes would need repeatable measurements and proof that admission,
revision, memory and fairness semantics remain intact. Existing CLOCK/TinyUFO
behavior and explicit worker overrides are retained.

## Reproduction and evidence

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
the two-thread load generator 3–4, origin CPU 5. The CPU governor and per-run
load averages are retained. Full-system windows are two seconds (body-size
windows one second), after warmup, across three interleaved trials; they are
sampling evidence, not release-grade isolated-host precision. Connections:
1, 8, 32, 100, 128, 512. Bodies: 1 KiB, 16 KiB, 64 KiB, 1 MiB, 4 MiB.
CLOCK and TinyUFO use identical budgets/workloads. Worker alternatives explicitly
set one and two workers against the affinity-derived default of three.

Raw data, comparison tables, binary identities and the final assessment follow
when campaign/validation output has been checked. The report does not declare
Stage A complete while that evidence is pending.
