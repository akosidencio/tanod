# Spike: build on `pingora-cache`, or build our own store and coalescer?

**Verdict: build on it.** Implement `Storage` ourselves; take the cache lock,
the `Vary` machinery, freshness and purge as given.

Run with `cargo test -p spike-pingora-cache`.

## What the three questions turned up

### 1. Can we own the store? Yes.

`MemCache` is the only `Storage` implementation and is documented "for testing
only, not for production use." Reading it, the reasons are concrete and both are
ours to fix:

* it is an unbounded `HashMap` with no byte budget — a memory-exhaustion vector
  in the one component whose job is to absorb traffic spikes;
* `lookup_streaming_write` is `.expect("must have partial write in progress")`;
  the safe contract is to return `None` when the requested write tag vanished,
  never to substitute another body for the same key.

`BoundedStore` in this spike implements the same trait with a byte budget,
eviction, and a graceful fallback on both paths. It is ~300 lines. That is
dramatically less than reimplementing HTTP cache semantics.

**Friction to expect:** every `Storage` method takes `&'static self` (upstream
marks it `// TODO: shouldn't have to be static`), so the store is leaked once at
startup. The traits are `#[async_trait]`, so impls need the attribute or you get
a wall of confusing `E0195` lifetime errors. `CacheKey::combined()` comes from
the `CacheHashKey` trait, which must be imported.

### 2. Does coalescing destroy streaming? No — this was resolvable.

`Storage` has a first-class streaming-partial-write path: a miss handler
publishes a `streaming_write_tag`, and waiters call `lookup_streaming_write`
with it to attach to the write *in progress*.

`question_2_a_waiter_gets_the_first_chunk_before_the_render_finishes` proves a
waiter receives the leader's shell immediately, then the closing chunk when the
leader emits it — rather than waiting for the whole render. The 20× TTFB
regression that a wait-for-completion design would have inflicted on every
waiter is avoidable.

### 3. Can we collapse without retaining? Effectively, yes — but not the way the spec described.

pingora-cache will not share a response it considers uncacheable. Its lock
raises `LockStatus::GiveUp`, documented as *"the writer observed that no cache
lock is needed (e.g. uncacheable), readers should start to fetch independently
without a new writer."*

That is **safe** — an uncacheable response is never fanned out, which is
precisely the leak the spec left undefined. But its answer to the herd is to
release every reader to the origin at once. Upstream flag it themselves in
`ReadLock::wait`: *"need to be careful not to wake everyone up at the same
time."*

The production implementation therefore uses a short-lived **fresh in-flight
entry** marked as transient. Followers may read it while the leader is active,
and the storage removes it instead of admitting it when the leader finishes.
An entry born stale does not work through Pingora's full stale-lock state
machine, even though a storage-only prototype can read its partial body.

## What this means for the architecture

The layering now has a clear division:

```
pingora-cache   lock, freshness, Vary, purge, streaming write
     +
tanod         Storage impl, cache key, shareability, admission
```

Admission control earns its place twice over: the `GiveUp` path releases an
unbounded herd to the origin, and our limiter is what bounds it. That is a
concrete argument for leading with the governor, not the cache.

## Still open

* **The temp buffer is unbounded during a write.** `max_body_size` is enforced
  at `admit()` — after the body is fully accumulated. A response that streams
  past the budget still buffers in full before being refused. Needs a running
  check in `write_body` that abandons the recording while letting the client's
  stream continue.
* **Eviction is FIFO.** Fine for a spike; the real one should use TinyUFO
  (`pingora-lru` is already in the tree).
* **`purge` keys on `CompactCacheKey::combined()`**, which must be verified to
  match what `lookup` stores under, or purge silently no-ops.
* **Leader disconnect.** A dropped `WritePermit` unlocks as
  `LockStatus::Dangling` behind a `debug_assert!(false, ...)`, so upstream
  treats leader-disappearance as close to a bug. This does not remove the need
  for a detached, owned fetch task; it confirms it.
