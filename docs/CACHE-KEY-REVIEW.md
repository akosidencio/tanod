# Cache keys and response shareability: a review brief

**Status: prepared for an independent review that has not yet happened.**

This document exists because "obtain an independent review of cache keys and
response shareability" is a roadmap item the author cannot complete alone. What
the author *can* do is make the review cheap to perform: isolate the code that
matters, state the claims it makes as falsifiable propositions, and list the
attacks already tried so a reviewer spends their time on the ones that were
not.

If you are reviewing this: the fastest way to be useful is to try to break one
of the eleven claims in §3. Each is stated so that a counterexample is
unambiguous.

---

## 1. Why these two components and nothing else

Tanod's failure modes divide cleanly. Most of them make it slow, or make the
origin do work it did not need to do. Two of them serve one person's data to
another:

* **`src/cache/key.rs`** — deciding that two requests are the same request.
* **`src/cache/policy.rs`** — deciding that a response may be handed to a
  request other than the one that produced it.

Everything else in the codebase can be wrong without a confidentiality
consequence. These two cannot. They are therefore written as pure functions
with no proxy runtime attached, which is also what makes them reviewable in
isolation: you need no Pingora knowledge and no running server to reason about
either.

Total surface under review: roughly 400 lines of logic across two files, plus
the classifier that feeds them (`src/classifier/`, ~350 lines).

---

## 2. How to read the code

Suggested order, about an hour:

1. `src/cache/key.rs` — `CacheKey`, `KeyBuilder::build`,
   `CacheKey::canonical_string`, `canonical_query`,
   `normalize_accept_encoding`, `normalize_host`.
2. `src/cache/policy.rs` — `evaluate_request`, then `evaluate_response`, then
   `unsupported_vary`.
3. `src/classifier/generic.rs` and `src/classifier/nextjs.rs` — what decides a
   request's class, and therefore its defaults.
4. `src/proxy/mod.rs` — `cache_key_callback` and `response_cache_filter`, the
   two places the pure logic is wired to the runtime. These are where a
   reviewer should look for the *inputs* being wrong rather than the logic.

The threat model is `docs/THREAT-MODEL.md`; §4.1 is the section this brief
elaborates.

---

## 3. The claims

Each claim is a proposition. A counterexample falsifies it.

### Key construction

**C1 — The canonical encoding is injective.**
Two `CacheKey` values that differ in any field render to different strings.
The encoding is length-prefixed rather than separator-delimited, so this should
hold for arbitrary component contents, including components containing the
separator bytes themselves.
*Where:* `CacheKey::canonical_string`.
*Already asserted by:* a proptest, plus a constructed boundary-shift case
(`host="x␟y", method="z"` vs `host="x", method="y␟z"`) that a random proptest
provably cannot find. The `cache_key` fuzz target drives the same function.
*Known history:* a previous separator-only encoding was injective only because
`http` rejects control characters — a property of a dependency rather than of
this function.

**C2 — Absence and emptiness are distinct.**
`deployment: None` and `deployment: Some("")` render differently; so does a
header that is absent versus present-and-empty.
*Known history:* they did not. `unwrap_or("")` folded them together. Found by
the `cache_key` fuzz target.

**C3 — No header value can be lost on its way into the key.**
Every header that selects a variant reaches the key through
`classifier::header_text` (`escape_ascii`), which is injective over byte
strings and whose output is printable ASCII, so it can neither collide nor
contain a separator.
*Known history:* callers used `to_str().ok()` and dropped the value. Since
`HeaderValue::to_str` refuses obs-text that `HeaderValue` itself accepts, a
non-ASCII variant header read as "header absent" — so clients wanting different
variants shared an entry.

**C4 — The scheme in the key is always `http` or `https`, and is never
attacker-chosen.**
It comes from the connection unless a peer inside `server.trusted_proxies.from`
said otherwise, and is normalised to one of two values for everyone.
*Where:* `net/forwarded.rs`, `proxy::cache_key_callback`.
*Already asserted by:* unit tests, a proptest, the `forwarded` fuzz target, and
`bench/forwarded.sh` (seven invented scheme values against one URL must produce
one origin render).

**C5 — The authority always reaches the key, over either protocol version.**
*Where:* `proxy::request_host`.
*Known history:* it did not, over HTTP/2. There is no `Host` header in h2; the
authority is a pseudo-header Pingora surfaces on the URI. Reading only `Host`
gave every h2 request an empty host and merged every virtual host on the
listener into one entry. Found 2026-08-27 while writing `bench/http2.sh`; the
benchmark and a unit test both fail against the previous code.

**C6 — Content coding is part of the key.**
Otherwise a brotli entry eventually reaches a client that only reads gzip.
*Where:* the `~encoding` variant, `normalize_accept_encoding`.

**C7 — Client-controlled key cardinality is bounded where a route says so.**
`cache.query.mode: include` drops unlisted parameters; the canonical query is
sorted so ordering does not double entries.
*Caveat worth attacking:* the default is to include everything, so a route with
no query policy has an unbounded key space by design. Is defaulting that way
defensible, given the component's purpose is to reduce origin work?

### Shareability

**C8 — `Set-Cookie` is an unconditional refusal.**
No route configuration, no override, no class reaches past it.
*Where:* the first branch of `evaluate_response`.
*Already asserted by:* `bench/safety.sh` and the private-response section of
`bench/adversarial.sh` (every session cookie handed out under load is
distinct).

**C9 — An origin `Vary` naming anything outside the key is a refusal.**
Including `*`. `Accept-Encoding` is exempt because it is in the key by
construction.
*Where:* `unsupported_vary`. Driven by the `vary` fuzz target.

**C10 — `override_origin` is fenced.**
It requires an explicit `class:` and an explicit `ttl.max`; it is refused on a
private class; it does not reach `Set-Cookie` or `Vary`. When it applies it
disregards the origin's TTL wholesale rather than taking `min(origin, route)`,
because a Next.js dynamic route answers `max-age=0` and the minimum would pin
the result to zero — silently disabling the override the operator asked for.
*Worth attacking:* this is the single most dangerous configuration option in
the project. Is the fence sufficient?

**C11 — A response that cannot be stored is never quietly shared instead.**
The three outcomes are `Shareable`, `TransientOnly` (collapsed within one
flight, retained for nobody afterwards) and `NotShareable`. `TransientOnly` is
implemented in the store, not in the `CacheMeta`: the temporary entry is never
promoted and is dropped after a short handoff window.
*Worth attacking:* the handoff window is 25ms and exists because Pingora
releases the cache lock when the miss handler is created, so removing the
entry immediately races the woken followers. Is a time-based window the right
shape, and is 25ms defensible?

---

## 4. Attacks already tried

So a reviewer does not repeat them.

| Attack | Result |
| --- | --- |
| Random proptest for key collisions | **Found nothing, and provably could not** — a boundary-shift collision needs two keys built from the *same* text with the boundary moved, and independent draws never agree. Two such tests passed against deliberately-broken code before this was noticed |
| Constructed boundary-shift collisions | Found the separator-only encoding's weakness; now the regression test |
| Fuzzing `canonical_query`, `normalize_accept_encoding`, `unsupported_vary`, `CacheControl::parse`, cookie parsing, config parsing, forwarded resolution | Seven targets, in CI. Found the `deployment: None`/`Some("")` fold |
| obs-text in every header the classifier reads | Found four distinct bugs at once (draft mode hidden, Server Action reclassified as a document, prefetch made storable, variant collapsed to absent) |
| Query-parameter flooding, invented scheme values, forged `X-Forwarded-For` | `bench/forwarded.sh`, `bench/adversarial.sh` |
| Two authorities at one path over HTTP/2 | Found C5 |
| `Range`, `If-None-Match`, `HEAD`, truncated `Content-Length`, unterminated chunked | `bench/protocol.sh` |
| 20s of mixed hostile traffic against every mechanism at once | `bench/adversarial.sh` |

---

## 5. Where a reviewer's time is best spent

The author's own assessment of where this is weakest, in order:

1. **The undeclared-personalisation gap.** An origin that personalises a
   response without `Set-Cookie`, `Cache-Control: private` or a declared `Vary`
   is indistinguishable from a public one. Tanod's answer is the route class
   and `cache.vary`, both of which are the operator's judgement. Is there a
   detectable signal being missed? Is there a safer default than trusting the
   declared class?
2. **`override_origin`.** C10. The fence is four rules; is there a fifth?
3. **The `TransientOnly` handoff window.** C11. A 25ms window in which an entry
   exists that is meant not to. What can reach it in that window?
4. **The interaction between coalescing and shareability.** The decision to
   share is made *after* waiters have already attached to the flight. When the
   answer turns out to be `NotShareable`, Pingora's documented behaviour is
   `LockStatus::GiveUp` — safe, in that nothing is shared, but the waiters are
   released as an unbounded herd. Is "safe but a herd" the right reading?
5. **Cache-key inputs rather than cache-key logic.** The logic is tested; the
   wiring in `proxy/mod.rs` that supplies its inputs is where C5 was found, and
   is comparatively under-tested.

---

## 6. What this brief does not claim

That the components are correct. Only that these are the claims being made,
these are the attacks already run, and this is where the author believes the
remaining risk sits. An independent reviewer disagreeing with the framing in §5
would itself be a useful result.
