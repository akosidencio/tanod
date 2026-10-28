# Phase 3 performance, scalability and memory efficiency

Branch `perf/phase3-scalability`, from `origin/dev` at `eed23e1` (Phase 2,
PR #9, merged; Phase 1, PR #8, merged before it). Measured 2026-10-28. No
merge, push, tag or release. Every number below was measured on the machine
described in B; nothing is estimated.

## A. Executive summary

The largest bottleneck was not in Tanod's code. Tanod never set Pingora's
`threads`, so the proxy ran on Pingora's default of **one worker thread**
whatever the host: on a six-CPU machine 1 KiB cache hits stopped at about
22,000–24,000 requests per second with 0.9 cores busy and the other cores
idle. The new `server.threads` (default: one per available CPU) removes that
ceiling.

Inside the cache store, four measured bottlenecks were fixed:

* every cache hit **copied the body** (`Bytes::copy_from_slice` per 64 KiB);
* every fill **completion took the lifecycle lock exclusively**, stopping all
  lookups, and every body chunk took all four cache write locks;
* under CLOCK, every **refill of a stored key scanned the whole eviction
  queue** under every cache write lock (2.2 ms per refill at 100,000 entries);
* route resolution was a **linear glob scan** (130–150 µs per request with
  1,000 routes, which `@tanod/next` can generate for a large site).

Results (medians of three interleaved trials; full tables in E):

| | Baseline | Optimized |
|---|---|---|
| 1 KiB hit throughput, 3 CPUs, 32–512 connections | 21–23k rps | 44–58k rps |
| 4 MiB hit, in-process store, one thread | 3,700/s | 164,000/s |
| Peak RSS serving 1 MiB / 4 MiB hits | 72 / 140 MiB | 57–66 / 100–114 MiB |
| CLOCK refill, 100k entries cached | 2.1 ms | 10 µs |
| Route resolution, 1,000 routes, worst case | 132–154 µs | 0.46–0.52 µs |
| Coalescing, 10–1000 identical cold requests | 1 render | 1 render |

The price, stated plainly: with three worker threads instead of one, Tanod
spends 7–30% more CPU per small request (more at high concurrency on very
large bodies) to deliver 2–2.7× the throughput. With `threads: 1` the store
changes alone are neutral to positive on CPU (C, E3). One optimization —
caching decoded response headers — was implemented, measured and **rejected**
for its memory cost (I).

## B. Baseline environment

| | |
|---|---|
| CPU | Intel Core i5-9400F, 6 cores, no SMT; governor `powersave` (cannot be changed without root) |
| Memory | 16 GiB |
| OS | Fedora, Linux 7.2.10-200.fc44.x86_64, inside the VS Code Flatpak sandbox |
| Toolchain | rustc 1.98.0 (`rust-toolchain.toml`), `--locked` |
| Build | `--release`: `lto = "thin"`, `codegen-units = 1` |
| Key dependencies | Pingora 0.9.0, tokio 1.53.3 (held below 1.54, see Phase 2), parking_lot 0.12.5, bytes 1, globset 0.4.20, TinyUFO 0.9.0 |
| Pinning (full-system runs) | Tanod CPUs 0–2, load generator 3–4, origin 5 (`taskset`) |
| Load generator | `bench/loadgen` (new, closed-loop HTTP/1.1 keep-alive, 2 threads) |
| Origin | `bench/slow-origin` (render delay + optional render slots), one connection per request |
| Logging | default (`info`, JSON access log to a file) |
| Binaries | baseline `ec9e30ef…` (eed23e1); optimized `9a3c95aa…` (store2) and `930e0534…` (final, see H); every run records its binary's SHA-256 in `environment.json` |

**Machine noise.** Other workloads shared the machine — another session's
TalaDB `cargo test` including an aarch64 cross build, and VS Code — with load
averages of 13–24 during the first campaign. Those results (C) are used only
for the profile, never for a headline number. Every comparison in E was run
interleaved trial by trial (`bench/perf-ab.sh`) in quieter windows (load
average 2–5, partly from the benchmark itself). Even so, multi-threaded
throughput on this desktop varies ±30–50% between runs of *identical* code
(two builds with byte-identical hit paths measured 20.6k and 32.9k rps for 1
KiB at c128). Kernel-accounted CPU time per request varied about ±3% and is
the more trustworthy efficiency figure; every table shows min–max beside each
median.

## C. Profiling findings

**Single-thread ceiling (confirmed).** Baseline, 1 KiB hits: 15.5k rps at 1
connection, 24.4k at 8, 24.1k at 32, 22.6k at 128 — flat — with Tanod at
0.67–0.90 cores. `ServerConf::default()` has `threads: 1` and Tanod never
overrode it.

**Flat profile of that thread** (`perf record -e cpu-clock`, user space only:
call-graph sampling and attaching to a running pid are refused in this
sandbox), 1 KiB hits at c32:

| Share | Where |
|---|---|
| 16% | allocator (`malloc`, `_int_malloc`, `free`, …) |
| 12.5% | HTTP header maps and parsing |
| 10.5% + 1.5% | zstd decompression and re-parsing of the cached response header, and the internal metadata record — on **every hit** (`CacheMeta::deserialize`) |
| 7.6% | access-log formatting, JSON escaping and env_logger's ANSI stripping (`anstream` 2.4%) |
| 6.8% | `memmove`/`memcmp` |
| 3.3% | blake2 key hashing and key construction |
| 1.8% | Prometheus label lookups |
| 1.1% | the store's own code, 1.2% route/classify |

**Store microbenchmarks** (`bench/hotpath.rs`, in process, tasks on
`min(concurrency, 6)` worker threads, baseline code):

* Hit cost grew with body size because of the copy: 1 KiB 250k/s, 64 KiB
  150k/s, 1 MiB 14k/s, 4 MiB 3.7k/s on one thread.
* Fills collapsed with concurrency: CLOCK fill-only 75k/s on one task, 105k at
  8, 14–42k at 32–512 tasks; hits mixed with fills likewise.
* `refill/clock`: 16 µs at 1k entries, 90 µs at 10k, **2.2 ms at 100k** —
  `order.retain` on every replacement, under all four cache write locks.
  TinyUFO: flat 12.5 µs.
* Route resolution: 90 ns for the first route regardless of table size; last
  or catch-all 0.3 µs (5 routes), 1.4 µs (25), 6–7 µs (100), 64–69 µs (500),
  124–154 µs (1,000).
* `CacheMeta` per hit: 3.0 µs to decode both halves; 1.0 µs to decode only the
  internal record and clone an already-decoded header.

**Lock contention.** No lock tracer is available (no root, no call graphs),
so contention is measured by scaling: hits scaled 4.8–5.2× from 1 to 8
workers on 6 cores (read locks only: `lifecycle`, `cached`, `tiny`); fills did
not scale at all, because of the exclusive `lifecycle` lock on completion and
the four write locks per body chunk. Those were the locks to fix.

**Allocations per request** (`bench/alloc.sh`, DHAT, 400 requests per type,
startup subtracted, baseline): static hit 126.5 (51 KB), SSR hit 138.7
(77 KB), SSR miss 272.7 (243 KB), private 146.0, RSC hit 145.7, Server Action
136.2, bypass 200.8, shed 98.7. Of an SSR hit's ~135, Pingora's hit path is
29, the store lookup 28 (almost all `CacheMeta` decoding) and **cache-key
construction about 35**, including 9 temporary strings for length prefixes.

**Rejected hypotheses.** Sharding the cache map: hits already scale on read
locks and were never the contended path. Contention on `used`: once
reservations stopped taking the map locks it no longer shows. A routing trie:
a `GlobSet` filter preserves glob semantics exactly with far less code.

## D. Implemented optimizations

### D1. Proxy worker threads (`server.threads`)

* **Root cause:** Pingora's default of one thread per service, inherited.
* **New:** `server.threads`, default `available_parallelism()` (honours
  affinity and cgroup quotas), set on the proxy service only — the admin and
  metrics services keep one. Validated 1–1024; a reload that changes it is
  refused; printed at startup.
* **Concurrency:** Pingora's work-stealing runtime; every store and limiter
  structure was already `Sync`. This is what turns the store's locks into
  shared state, which is why D2–D4 matter.
* **Behaviour change:** the default changes from 1 to the CPU count,
  documented like `cache.eviction`'s; `threads: 1` restores the old behaviour.

### D2. Zero-copy cache bodies

* **Root cause:** bodies were `Arc<RwLock<Vec<u8>>>`; every hit and every
  follower copied each chunk out under the read lock, and the writer's
  `extend_from_slice` could reallocate the whole body under the write lock
  while followers waited.
* **New:** a fill's body is a list of immutable `Bytes` segments plus an
  unsealed tail under 4 KiB (`FillBody`). Writes of 4 KiB or more are copied
  once, *before* the lock, into their own exact-size segment; smaller writes
  accumulate in the tail. Readers keep a cursor (offset, segment index) and
  slice sealed segments without copying; only the tail is copied. On
  completion the body is frozen into `Frozen` (immutable, read with no lock at
  all) and compacted into 64 KiB segments, once, if its segments would make a
  hit take more downstream writes than a contiguous body.
* **Why always copy writes:** an HTTP/2 upstream chunk can be a slice of a
  larger receive buffer; keeping it would hold the whole buffer behind a few
  accounted bytes. Pingora's HTTP/1 reader already hands exact copies, but the
  store cannot tell which it got.
* **Concurrency:** segments never change once sealed, so a cursor's segment
  index stays valid; sealing the tail moves no byte to a new offset. The
  reader takes the body read lock only to slice or copy at most 4 KiB.
* **Memory:** accounting unchanged (body bytes). Real overhead is one `Bytes`
  and its shared header per segment, under 2% at the 4 KiB floor; tiny writes
  cannot multiply it. Detached readers still keep segments alive after
  eviction, as before.
* **Compaction rule was corrected after measurement:** the first version
  allowed up to twice the minimum number of segments; a 16 KiB page arriving
  in two upstream reads then cost two downstream writes per hit and measured
  3–7% slower (E3). The rule is now "no more writes than contiguous".

### D3. Completion and reservation without global exclusion

* **Root cause:** `FollowableMiss::finish` held `lifecycle.write()` to move a
  fill into `cached` without a visible gap, so every completion stopped every
  lookup; `reserve_fill` took `cached`, `order`, `tag_index` (and `tiny`) for
  write on every body chunk even when nothing needed evicting.
* **New:** completion takes `lifecycle.read()`, which still excludes purge and
  namespace retirement (they take the write side), and admits the entry to
  `cached` *before* removing the fill from `temp`. A lookup that misses
  `cached` and then `temp` checks `cached` once more: missing `temp` means a
  racing completion has already admitted. Reservations that fit the budget
  take `used` alone.
* **A race found and fixed during validation.** The first version relied on
  admit-before-remove alone; a lookup could still read `cached` just before
  the admission and `temp` just after the removal. The new stress test
  `spinning_lookups_never_miss_a_completing_fill` found 9 such misses in 2,000
  completions without the second check and none with it (verified both ways).
  The store2/final binaries in E predate this fix; it adds one map read only
  to lookups that miss entirely, and a matched check (`final` vs `final2`)
  showed miss CPU per request −0.9% and −4.6%, coalescing still one render.
* **Lock order** (documented on `reserve_fill`): `lifecycle` → `used` →
  `cached` → `order` → `tag_index` → `tiny`; `temp` is never held across any
  of them during completion. No lock is held across an await.
* **Atomics:** `pinned` is still written only under `used`; `Relaxed` is
  enough because `used` orders the writers and readers only need a recent
  value for status and the advisory `accepting_fills`.

### D4. Lazy CLOCK/FIFO eviction queue

* **Root cause:** replacing an entry ran `order.retain(|k| k != key)`, O(n)
  under every cache write lock.
* **New:** queue records carry a ticket (the entry's `revision`, unique per
  admission). A record whose entry was replaced or purged is skipped when it
  reaches the front; the queue is compacted when records exceed
  `2 × entries + 64`. Victim order is exactly that of the eager queue
  (`a_replacement_is_not_evicted_through_its_old_queue_record`), and the
  queue stays bounded when nothing is evicted
  (`retired_queue_records_stay_bounded`).

### D5. Indexed route resolution

* **New:** tables of 16 routes or more build one `GlobSet` from the routes'
  own `Glob`s. It returns every matching index in ascending order; each
  candidate is confirmed by its own matcher (host, methods, path) and the
  first wins, so the result is exactly the linear scan's. A differential
  property test checks this over random tables from the glob grammar the
  routes use (exact, `*`, `?`, `**`, alternation, classes) with host and
  method constraints. Candidate indices live in a thread-local buffer, so
  resolving allocates nothing.
* **Trade-off:** when the *first* route matches, large tables now pay 270–480
  ns instead of 90 ns. Below 16 routes the scan is kept.

### D6. Cache-key construction allocations

* `canonical_string` formats each length prefix into the key instead of
  through a temporary `String` (9 fewer allocations); the namespace prefix and
  canonical form are rendered into one buffer (`CacheNamespace::key_for`);
  variant header names are lowercased only for headers the request carries;
  `cache_key_callback` borrows the path and query. **The key bytes are
  unchanged**, pinned by `the_canonical_encoding_is_byte_for_byte_stable`
  (golden string and identical Pingora hash). Not separately benchmarked
  end to end; the DHAT figures in E are from the build before it.

## E. Before/after results

### E1. Headline table (default threads = 3 under the 3-CPU pin)

Full-system figures are medians of three interleaved trials, min–max in
brackets; campaign `c4` (hit, sizes, miss; binary `final`) and `c2`
(everything else; binary `store2`, whose hit path differs only in the
compaction rule).

| Metric | Baseline | Optimized | Difference |
|---|---|---|---|
| Cache-hit RPS, 1 KiB, c32 | 22,783 (22,657–23,696) | 52,744 (42,794–64,408) | +131% |
| Cache-hit RPS, 1 KiB, c512 | 21,124 (20,922–21,364) | 47,714 (35,290–51,978) | +126% |
| Cache-hit p99, 1 KiB, c32 | 2,435 µs | 1,653 µs | −32% |
| Cache-hit p99, 1 KiB, c512 | 41,355 µs | 23,035 µs | −44% |
| Cache-hit p99, 1 KiB, c8 | 494 µs (448–814) | 1,226 µs (338–2,696) | +148%, see J |
| Tanod CPU per 1 KiB hit, c1 / c32 / c512 | 44.5 / 38.5 / 41.9 µs | 45.0 / 41.5 / 48.2 µs | +1% / +8% / +15% |
| Tanod CPU per 1 KiB hit, `threads: 1` (E3) | 44.9 / 37.0 / 40.1 µs (c1/c8/c128) | 43.4 / 37.5 / 40.5 µs | −3% / +1% / +1% |
| Peak RSS, 1 MiB hits, c1–c100 | 71,852 KiB | 65,696 KiB | −9% |
| Peak RSS, 4 MiB hits, c1–c100 | 140,064 KiB | 113,432 KiB | −19% |
| Peak RSS, 1 KiB hits, c512 | 36,496 KiB | 41,556 KiB | +14% (threads) |
| Allocations per SSR hit (32 KiB) | 138.7, 76,546 B | 135.9, 43,603 B | −2%, −33 KB |
| Allocations per SSR miss | 272.7, 242,753 B | 284.7, 244,438 B | +4% |
| Streaming TTFB, max over 100-reader herd | 6,667 µs (3,238–50,145) | 6,322 µs (2,887–7,184) | −5% |
| Coalescing origin renders, 10/100/500/1000 identical | 1/1/1/1 | 1/1/1/1 | unchanged |
| Coalesced 1,000-request burst, p99 completion | 383 ms | 330 ms | −14% |

### E2. Cache hits by body size and readers (`sizes`, c4)

| Body | Readers | RPS baseline → optimized | p99 µs | CPU µs/req | Peak RSS KiB |
|---|---|---|---|---|---|
| 1 KiB | 1 / 10 / 100 / 500 | 14,631→14,566 / 23,995→58,090 / 22,212→40,985 / 21,015→44,201 | 149→141 / 624→459 / 4,904→5,335 / 68,881→22,737 | 43.5→44.5 / 37.3→40.9 / 40.1→47.0 / 42.3→48.9 | 12.5k→13.0k … 34.4k→38.0k |
| 16 KiB | 1 / 10 / 100 / 500 | 12,928→12,999 / 20,030→40,620 / 19,303→29,617 / 19,058→32,984 | 160→159 / 623→1,160 / 5,826→10,598 / 56,640→33,069 | 49.6→48.9 / 43.2→46.9 / 45.0→54.5 / 45.4→56.2 | 34.6k→36.4k … 56.7k→59.4k |
| 64 KiB | 1 / 10 / 100 / 500 | 11,000→10,873 / 16,483→41,628 / 14,901→30,764 / 15,027→23,129 | 213→214 / 1,094→988 / 10,813→6,085 / 94,888→50,246 | 57.8→57.2 / 51.3→52.5 / 56.9→68.1 / 56.8→72.7 | 56.4k→58.1k … 57.9k→67.2k |
| 1 MiB | 1 / 10 / 100 / 500 | 2,755→2,883 / 2,922→4,598 / 2,775→3,512 / 2,691→3,631 | 478→684 / 4,903→6,511 / 41,658→75,662 / 385,625→286,058 | 277→258 / 275→364 / 290→463 / 299→480 | 71.9k→65.7k … 74.5k→76.1k |
| 4 MiB | 1 / 10 / 100 / 500 | 799→835 / 822→1,493 / 770→1,103 / 624→857 | 1,556→1,827 / 15,489→17,322 / 246k→262k / 1.23 s→1.43 s | 975→918 / 969→1,235 / 1,029→1,632 / 1,261→2,155 | 140.1k→113.4k … 144.6k→127.6k |

### E3. Store changes alone (`threads: 1` on both, campaign c3, store2)

| Body, readers | RPS | CPU µs/req | Peak RSS KiB |
|---|---|---|---|
| 1 KiB, 1 / 10 / 100 / 500 | +0.1% / +1.0% / +2.5% / −1.2% | −0.3% / −1.0% / −1.0% / +1.3% | ±0.7% |
| 16 KiB, 1 / 10 / 100 / 500 | −7.4% / −3.4% / −5.0% / −2.6% | +5.8% / +2.5% / +3.6% / +1.0% | ±2% |
| 64 KiB, 1 / 10 / 100 / 500 | +8.4% / +2.2% / +7.1% / +3.0% | −5.4% / −3.8% / −6.7% / −2.5% | +0.4% / −1.1% |
| 1 MiB, 1 / 10 / 100 / 500 | +6.9% / +9.4% / +12.9% / +10.4% | −6.0% / −9.2% / −11.9% / −9.6% | −21.5% … −13.6% |
| 4 MiB, 1 / 10 / 100 / 500 | +12.9% / +8.8% / +13.3% / +21.1% | −11.5% / −8.3% / −12.7% / −17.0% | −28.3% … −18.4% |
| Miss, 16 KiB, c8 / c64 | −2.1% / −1.6% | +5.7% / +1.6% | +1% |

The 16 KiB row is the two-segment regression D2 describes; with the corrected
compaction rule, the same 16 KiB c1 case measured +0.6% RPS and −1.5% CPU per
request (c4).

### E4. Store microbenchmarks (baseline vs store2, 3 interleaved trials)

| Scenario | Baseline | Optimized | Difference |
|---|---|---|---|
| Hit 1 KiB, 1 / 8 workers | 252k / 1.30M ops/s | 254k / 1.32M | +1% |
| Hit 64 KiB, 1 / 8 | 154k / 814k | 259k / 1.34M | +68% / +64% |
| Hit 1 MiB, 1 / 8 | 13.9k / 38.3k | 226k / 1.19M | 16× / 31× |
| Hit 4 MiB, 1 / 8 (store1 run) | 3.7k / 6.7k | 164k / 842k | 44× / 126× |
| CLOCK fill-only, 1 / 8 / 128 / 512 tasks | 76.8k / 109k / 85.7k / 59.7k | 69.2k / 287k / 257k / 211k | −10% / +164% / +200% / +253% |
| CLOCK fills with 90% hits, 8 / 128 / 512 | 494k / 379k / 319k | 835k / 841k / 830k | +69% / +122% / +160% |
| TinyUFO fill-only, 8 / 128 / 512 | 71.2k / 6.3k / 3.9k | 108k / 23.3k / 3.5k | +52% / +269% / −11% (wide spread) |
| 10 coalesced followers, 1 MiB stream | 0.79 ms | 0.57 ms | −28% |
| 500 followers | 8.2 ms | 8.5 ms | +3% |
| CLOCK refill, 1k / 10k / 100k entries | 16.6 / 89.2 / 2,105 µs | 9.4 / 9.9 / 10.0 µs | −44% / −89% / −99.5% |
| Route, 100 routes, catch-all | 6,837 ns | 294 ns | −96% |
| Route, 1,000 routes, last section | 128,600 ns | 518 ns | −99.6% |
| Route, 1,000 routes, first route | 95 ns | 481 ns | +407% (+386 ns) |
| Route, 5 routes | 94–306 ns | 96–315 ns | +3% (scan kept) |

## F. Correctness regression results

All on the final tree, Rust 1.98.0, `CCACHE_DISABLE=1`.

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| same with `--features tls` | Passed |
| `cargo test --workspace --all-targets --locked` | Passed: 580 library, 7 binary, 6 spike, 3 slow-origin, 0 loadgen; `eviction` and `hotpath` bench targets ran in smoke mode |
| `cargo test --workspace --lib --locked --features tls` | Passed: 581 + 6 |
| `cargo test --workspace --doc --locked` | Passed (no doctests) |
| `cargo audit -D warnings` | Exit 0 |
| `git diff --check` | Passed |
| `./bench/all.sh` (CLOCK and TinyUFO, Docker, TLS) | All benchmarks held: 39 runs, 52 `PASS`, none skipped (G) |

New tests: `src/cache/store_phase3_tests.rs` (18: body chunking, a proptest
round trip, zero-copy pointer identity across hits, compaction, seeking,
stored-completion gap under a frozen clock and under spinning OS threads,
completion/purge races with consistent accounting, fast/slow reservations
never overcommitting, the lazy queue's victim order and bound); route-index
differential proptest; golden cache-key encoding; `server.threads`
validation, default and reload refusal.

## G. Load-testing results

**Origin overload** (`overload`, c2): an origin rendering 8 at a time in 50 ms
(160 renders/s), every request uncached dynamic work.

| Connections | Direct to origin | Through Tanod, baseline | Through Tanod, optimized |
|---|---|---|---|
| 8 / 32 | origin-bound, latency grows with queue | 150 / 147 rps, origin peak 8 | 150 / 147 rps, origin peak 8 |
| 128 / 512 | origin queues every request | 16.0k / 16.3k rps, ~96k / 97k shed (503), p99 7.1 / 66 ms | 21.3k / 20.9k rps, ~127k / 125k shed, p99 5.9 / 43 ms |

The origin never exceeded its ceiling of 8 in any run, with no errors: the
optimized build sheds the excess faster and more cheaply, it does not admit
more. (Direct rows are in `overload.jsonl`; they are a different system, not a
proxy-overhead comparison — see `passthrough`.)

**Mixed production traffic** (`mixed`, c2; 40% cacheable SSR over 1,000 pages,
20% static, 20% private with cookies, 10% RSC, 10% uncached search; origin 64
slots × 10 ms): 10,870 → 16,756 rps (+54%), p99 34.1 → 31.8 ms, zero errors;
origin renders rose with throughput (27,252 → 39,950) because 30% of the mix
is never cacheable.

**Forwarding overhead** (`passthrough`, observe mode, no cache): 7,068 →
13,650 rps at c64; CPU per request 109 → 126 µs.

**Slow streaming** (`stream`, eight chunks 150 ms apart, a tenth of the herd
reading at 16 KiB/s): one render for 10, 100 and 500 readers; the fast
readers' p99 equals the leader's render time on both builds (455–494 ms), so
slow readers did not hold anyone back.

**Memory pressure** (`pressure`, 400 × 256 KiB keys over a 16 MiB budget, then
200 × 1 MiB with 20 slow readers): 2,584 → 4,965 rps (+92%), zero errors,
peak RSS 47.8 → 48.4 MiB; burst fast-reader p99 73.7 → 61.0 ms.

**End-to-end suite.** `TMPDIR=/tmp/claude-1000/u ./bench/all.sh` on the
final tree: **all benchmarks held** — 39 script runs, 52 `PASS` lines, nothing
skipped, in 14 minutes. That covers coalescing, streaming, protocol, purge,
cache-policy reload, memory, cache pressure, storage and the 45–60 s soak under
both CLOCK and TinyUFO, plus forwarded headers, HTTP/2, spool, adversarial,
admin, upgrade (socket handover), supervise, tracing, chaos, breaker (both
strategies), retry, WebSocket and TLS. The real Next.js tests — `nextjs.sh`
(11 checks), `nextjs-forwarded.sh` and `nextjs-browser.sh` (Playwright) —
ran against a Tanod image rebuilt with `docker compose ... build --no-cache`
from the final tree and lockfile (image `d70d9f1692a8`), closing the stale-image
gap Phase 2 reported. The suite's scripts build the debug profile, as in CI.

**Not run in this phase:** the one-hour soak and the release-parameter memory
runs from `RELEASE-GATES.md`, the per-version Next.js matrix
(`bench/nextjs-compatibility.sh`, which `all.sh` does not include), and fuzz
campaigns (cargo-fuzz is not installed).

## H. Changed files

| File | Purpose |
|---|---|
| `src/cache/store.rs` | `FillBody`/`Frozen` segmented bodies and cursors, zero-copy `CompleteHit`/`PartialHit`, completion under the lifecycle read lock with admit-before-remove and the second `cached` check, reservation fast path, lazy `EvictionQueue` |
| `src/cache/store_phase3_tests.rs` | New store tests (F) |
| `src/cache/key.rs` | Allocation-free length prefixes, `CacheNamespace::key_for`, lazy variant-name lowercasing; golden encoding test |
| `src/proxy/mod.rs` | Borrowed path/query in `cache_key_callback`, `key_for` |
| `src/policy/matcher.rs`, `src/policy/mod.rs` | `RouteIndex` and indexed `resolve`; differential proptest |
| `src/config/schema.rs`, `src/config/validation.rs`, `src/policy/reload.rs`, `src/main.rs` | `server.threads`: field, validation, reload refusal, proxy-service wiring, startup line |
| `bench/loadgen/` (new crate), `Cargo.toml`, `Cargo.lock` | Load generator; workspace member and `hotpath` bench target. The lockfile gains only the `loadgen` package: no new external crate |
| `bench/perf.sh`, `perf-ab.sh`, `perf-compare.py`, `perf-cache.yaml.tpl`, `perf-mixed.yaml.tpl`, `perf-mixed.mix` | Full-system scenarios and interleaved comparison |
| `bench/hotpath.rs`, `bench/hotpath-compare.py` | Microbenchmarks and their comparison |
| `bench/alloc.sh` | DHAT allocations per request |
| `bench/slow-origin/src/main.rs` | `.../size/<bytes>` bodies, `SLOW_ORIGIN_SLOTS` |
| `docs/OPERATIONS.md`, `docs/CONFIG-SCHEMA.md`, `docs/RELEASE-GATES.md`, `CHANGELOG.md` | Worker threads, body storage, the threads default as a behaviour change, performance tooling |
| `.gitignore`, this report | The report is tracked, unlike Phases 1–2: the operations guide cites it |

## I. Rejected optimizations

* **Caching each entry's decoded response header** (implemented, measured,
  reverted). It cut a hit's metadata cost from 3.0 µs to 1.0 µs. A decoded
  8-field SSR header occupies **2,703 bytes in 40 allocations** (DHAT, 20,000
  headers) against 270 bytes compressed; charged honestly, the budget holds
  14% fewer 16 KiB pages and roughly a third as many 1 KiB RSC payloads, and
  uncharged it would be unaccounted RSS growing with entry count. About 4–5%
  of a hit's CPU was not worth either.
* **Avoiding env_logger's ANSI stripping** (2.4% of hit CPU). It needs a
  hand-written formatter reproducing env_logger's default output exactly,
  including continuation indentation, on a log format operators parse. Left
  for a change that can be reviewed as a logging change.
* **Sharding the cache map, or replacing CLOCK/TinyUFO:** no evidence either
  was a bottleneck once D3–D4 were in.
* **A routing trie:** D5 gets the same worst case without new semantics.
* **Larger read slices than 64 KiB on hits:** would change how much a slow
  reader must accept within `timeouts.downstream_write`.

## J. Remaining bottlenecks

* **CPU per request with several threads** rises 7–30% for small requests and
  up to 60–70% for 1–4 MiB hits at 100–500 readers, against 2–2.7× the
  throughput. Candidates to measure next: Pingora's `work_stealing: false`
  (one runtime per thread), and tokio's idle spinning.
* **Tail latency at moderate concurrency** (c8–c100) is noisier with
  several threads: some runs improved p99, others worsened it (1 KiB c8: 494 →
  1,226 µs median, 338–2,696 µs spread). High concurrency improves
  consistently. This deserves a quieter machine and longer runs before a
  conclusion either way.
* **TinyUFO eviction under hundreds of concurrent fills** stays slow (about
  3.5–4k fills/s at 512, both builds): `TinyPolicy::evict` pads the gap between
  the policy's weight and its capacity with pressure tokens, up to 257 per
  victim, when most budget is pinned by fills the policy cannot see.
* **Per-request allocations:** about 135 per hit — Pingora's hit path (29),
  `CacheMeta` decoding (28), the access log, Prometheus label lookups. The
  metadata decode is the largest piece Tanod controls (see I).
* **Access logging** remains about 8% of hit CPU (I).

## K. Git state

* Branch `perf/phase3-scalability`, from `origin/dev` `eed23e1`; tracks
  `origin/dev`; nothing committed, pushed or merged. Local `dev` (`7ae34f9`)
  is behind `origin/dev` and was left untouched; `main` untouched.
* Modified and new files: H.
* Tests: F, all passing.
* Benchmark artifacts: `benchmark-results/perf/<label>/` (ignored), the
  session's raw campaigns and microbenchmark JSON; reproducible with the
  commands in `docs/RELEASE-GATES.md#performance-measurement`.
* Review readiness: ready for review. Points to look at first: the completion
  ordering and second lookup (D3), the `server.threads` default change (D1),
  and whether this report should stay tracked.
