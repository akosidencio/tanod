# Cache and response-sharing security review package, v0.4.0

**Status: maintainer-side review complete. Independent review NOT obtained.**

This package was prepared by the implementation side of the project, the
maintainer working with an AI coding agent. It is **not an independent
audit**. It exists so that a separate reviewer can check the work quickly, and
it records where that reviewer's approval belongs (§10). Until §10 is filled
in by someone who did not write this code, the "independent cache-key review"
release gate stays open.

Scope: candidate branch `release/v0.4.0-review`, from `dev` at `a4fe212`. The
claims in [CACHE-KEY-REVIEW.md](../CACHE-KEY-REVIEW.md) (C1–C11) remain the
reference; this package covers the four open topics (C7, C10, C11 and
`coalesce.override_origin`) and what investigating them found.

Two evidence labels are used throughout:

- **Verified under tested assumptions**: a test in this repository exercises
  the behaviour and fails if it regresses. Its assumptions are stated.
- **Not independently verified**: an argument from code reading, or a test
  the maintainer side wrote and checked by itself only.

Everything in this document is the second kind with respect to independence.
The first label describes test coverage, not review independence.

---

## 1. Summary

| Topic | Outcome |
|---|---|
| `coalesce.override_origin` | **Vulnerability found and fixed (P0).** A route's cache could store one visitor's response for the origin's `max-age` and serve it to other visitors. Fixed; the override is now fenced like `cache.override_origin`. |
| C7: unbounded query-key space | No unbounded resource found. Cost is bounded by the cache budget and the origin ceiling. **One related vulnerability found and fixed (P1):** query policies matched parameter names as sent, the origin decoded them, and an attacker's page could be stored under the bare URL (cache poisoning). |
| Cache-key inputs (found while investigating C7/C10) | **Vulnerability found and fixed (P1).** Paths with `.`/`..` segments were routed and classed as sent while Next.js resolved them. A private page could be stored under a public route, and a dynamic page classed `static` bypassed the origin ceiling. Such targets are now refused. |
| C10: `cache.override_origin` fence | No bypass of the fence found. It is an operator contract that lifts the cookie bypass; what remains unconditional is listed in §5. |
| C11: 25 ms transient handoff | Purpose confirmed, bounded, closed by purge and retirement. No unsafe path found. Pinned by new coordinated tests. |

No confirmed cross-user response-sharing defect remains open on the
candidate, *under the tested assumptions in §8*. That is the maintainer
side's conclusion, not an independent one.

---

## 2. Threat model

Attackers are internet clients who control paths, queries, headers, cookies
and connection timing, and who may also be ordinary logged-in users. Assets:
another user's personalised response; the integrity of shared public
responses; the origin's concurrency ceiling. Trusted: operator configuration,
the origin's response headers (as statements of intent, not of correctness),
and peers inside `server.trusted_proxies`. Out of scope: Pingora's HTTP
parser (request smuggling), and an origin that personalises a response
without saying so on a route the operator declared public. That last case is
the documented contract and is restated in §5.

Two properties matter for sharing:

1. **Persistence.** A response stored in the cache can reach any later
   request with the same key.
2. **In-flight sharing.** Followers attach to a fill *only after* the leader's
   response headers have passed `evaluate_response`: the miss handler, and so
   the temporary entry, exists only after `response_cache_filter` declared the
   response cacheable. Before origin headers are known, only request-side
   rules apply (§3). Waiters on the cache lock receive no bytes from a
   response later found `NotShareable`; they are released (`GiveUp`) and go
   to the origin themselves.

---

## 3. Code paths

| Step | Where |
|---|---|
| Dot-segment refusal (new) | `proxy::request_filter` → `classifier::has_dot_segment` |
| Classification | `classifier::nextjs::NextJs::classify_request` → `classifier::generic::classify` |
| Route selection, declared vs inferred class | `PolicySnapshot::resolve`, `proxy::resolved_request_class` (declared wins, upgrade always wins) |
| Request eligibility, cookie bypass, overrides | `cache::policy::evaluate_request` |
| Key construction | `proxy::cache_key_callback` → `cache::key::KeyBuilder::build`, `canonical_query`, `form_decoded` (new) |
| Coalescing key | the same Pingora `CacheKey`, namespaced by `CacheNamespace` (policy generation) |
| Response shareability | `proxy::response_cache_filter` → `cache::policy::evaluate_response` |
| Leader/follower attachment | `BoundedStore::lookup`, `lookup_streaming_write`, `get_miss_handler` |
| Completion, handoff, cleanup | `FollowableMiss::finish`, `schedule_handoff`, `Drop` |
| Purge and retirement | `purge_tags/paths/all`, `Storage::purge`, `activate_namespace` |
| Reload compatibility | `PolicySnapshot::cache_compatible` (coalesce settings are in the projection) |

---

## 4. `coalesce.override_origin`: vulnerability (P0), fixed

### Finding

`evaluate_response(res, route, key_headers, coalesce_only, coalesce_override)`
skipped the `no-store`/`private`/`no-cache` refusals whenever
`coalesce_override` was set. The proxy passed `coalesce_only =
ctx.transient_only = !may_store`. With the route's cache enabled (the
default), `may_store` was true, so `coalesce_only` was false and the function
fell through to the origin's TTL:

- `Cache-Control: private, max-age=60` → `Shareable { ttl: 60s }`: **stored**.
- `no-store, max-age=60` → stored.
- `public, s-maxage=60` on a cookie-bearing request → stored. The override
  had also lifted the cookie bypass, and storage was not disabled with it.

The schema promised "nothing is persisted". Next.js's own dynamic header
(`max-age=0`) became `TransientOnly`, which is why the Next.js suites never
saw it. Any origin sending a positive `max-age` with `private` (a common
per-user browser-cache header) leaked.

### Reproduction (against `a4fe212`)

```sh
cargo build --release --locked --bin tanod
python3 bench/cache-isolation.py --skip-refusal --tanod target/release/tanod
#  FAIL sequential /co/pmax/p40 cookie-b    SHARED from user-a
#  FAIL sequential /co/nsmax/p41 cookie-b   SHARED from user-a
#  FAIL sequential /co/public/p48 cookie-b  SHARED from user-a
```

Route `/co/**` is `class: public_ssr` with `coalesce: {override_origin: true}`
and the default cache settings. User A requests, the render ends, then user B
(a different session cookie) requests the same URL 150 ms later and receives
A's body.

### Fix (`3165f3b`)

- `evaluate_response`: a refusal lifted by the coalescing override yields
  `TransientOnly`. It never yields `Shareable`, whatever the origin's
  `max-age`.
- `evaluate_request`: when the cookie bypass was lifted by the coalescing
  override alone, `may_store = false`. Only `cache.override_origin` may
  persist the response to a cookie-bearing request.
- Validation requires an explicit `class:` for `coalesce.override_origin`, as
  it already did for `cache.override_origin`. Previously an unclassified
  route was protected only because a cookie-bearing request is *inferred*
  private, which is a per-request inference rather than an operator
  statement. It now fails closed at startup.

### Security decision

**Safe only with the additional restrictions now implemented.** Within one
flight, a route declared `public_ssr` with this override shares a response
between users, cookie-bearing ones included, including a response the origin
marked `private`. That is its purpose: collapsing a herd onto one render of a
page the operator states is not personalised. It is the same operator
contract as `cache.override_origin`, now with the same explicit-class fence,
and with a window bounded to one render plus the 25 ms handoff. Whether
coalescing should lift the cookie bypass at all is put to the reviewer in §9.

**Migration:** configurations with `coalesce.override_origin` and no `class:`
are refused at startup. The `@tanod/next` generator always emits
`class: public_ssr` with the override, so generated policies are unaffected.

---

## 5. C10: `cache.override_origin`

**Assessment: the fence holds; it is an operator contract, not a detector.**
Not independently verified.

The override requires an explicit non-private `class:` and a `cache.ttl.max`.
It lifts the origin's `no-store`/`private`/`no-cache` and **lifts the cookie
bypass**: a cookie-bearing request on such a route is cacheable, and its
response is stored for `ttl.max`. The route class is the operator's statement
that the route's responses do not depend on who asked. If that statement is
false, the override shares one user's page with everyone. `bench/cache-isolation.py`
records exactly this as *permitted by contract* on `/cache/**`.

Unconditional protections, which no route configuration reaches past
(verified under tested assumptions; matrix in §8):

| Protection | Where | Test |
|---|---|---|
| `Set-Cookie` in the response → never shared or stored | `evaluate_response` first branch | proptest `set_cookie_is_never_shareable`; isolation `*/setcookie/*` |
| `Vary` naming a header outside the key (incl. `Cookie`, `*`) → refused | `unsupported_vary` | proptests; isolation `*/varycookie`, `*/varyx` |
| `Authorization` on the request → bypass | `evaluate_request` | isolation `auth-b` on every route |
| Next.js draft mode cookies → bypass, class private | `NextJs::in_draft_mode` | proptest `draft_mode_always_forces_a_bypass`; isolation `draft-b` |
| Next.js Server Action (`Next-Action`) → mutation, bypass | `NextJs::is_server_action` | proptest; isolation `action-b` |
| RSC / prefetch / HTML are distinct keys; prefetch never stored | `RSC_KEY_HEADERS`, `coalesce_only` | key unit tests; isolation `rsc-b`; Next.js fixture `/account` RSC |
| Non-safe methods → bypass | `evaluate_request` | unit tests |
| Uncacheable status → refused | `CACHEABLE_STATUSES` | proptest |
| Protocol upgrade → never HTTP-cached | `resolved_request_class` | unit tests |
| `private_dynamic` class + override → refused at startup | validation | unit tests |
| `cache.vary` on `Cookie`/`Authorization`/`User-Agent`/`*` → refused | validation | unit tests |
| Dot-segment request targets → 400 before routing (new) | `has_dot_segment` | proptest; isolation; §6.2 |

The schema comment that said validation "refuses ... a cookie-bearing route"
was wrong: no such check exists or can exist statically. It has been
corrected to say the override lifts the cookie bypass.

**Residual (§9 Q2):** undeclared personalisation on a declared-public route
is undetectable. There is no request signal Tanod could use. Next.js renders
a `cookies()`-reading page and any other dynamic page with the same headers.

---

## 6. C7: query-key cardinality, and key-input divergences

### 6.1 Cardinality: bounded (verified under tested assumptions)

With no `cache.query` policy every parameter is in the key, so `?x=<unique>`
mints a key, a fill and a render per request. `bench/query-cardinality.py`
measured it (16 MiB cache budget, ceiling 8, 20 ms renders, 32 connections,
15 s per phase, i5-9400F, `intenal_docs` evidence):

| Phase | Policy | Requests | Origin renders | Origin peak | Cache peak / budget | Pinned peak | Evictions | RSS peak | Hot-key p50 / p99 |
|---|---|---:|---:|---:|---|---:|---:|---:|---|
| hot set only | CLOCK | 597,491 | 100 | 8 | 1.66 / 16.78 MB | 0 | 0 | 16.1 MiB | 0.81 / 1.23 ms |
| + unique param | CLOCK | 10,466 | 5,664 | 8 | 16.77 / 16.78 MB | 0 | 4,655 | 34.0 MiB | 0.60 / 2.01 ms |
| + 2 KiB name & value | CLOCK | 10,372 | 5,611 | 8 | 16.77 MB | 4 KB | 4,602 | 35.3 MiB | 0.63 / 2.18 ms |
| + purge every 500 ms | CLOCK | 6,820 | 5,675 | 8 | 3.08 MB | 16 KB | 0 | 33.9 MiB | 80.6 / 94.5 ms |
| `include: [p]` | CLOCK | 594,406 | 100 | 8 | 1.66 MB | 0 | 0 | 33.1 MiB | 0.81 / 1.24 ms |
| + unique param | TinyUFO | 10,461 | 5,659 | 8 | 16.77 MB | 16 KB | 4,650 | 37.0 MiB | 0.61 / 1.90 ms |
| + 2 KiB name & value | TinyUFO | 10,342 | 5,600 | 8 | 16.77 MB | 16 KB | 4,591 | 38.6 MiB | 0.63 / 2.05 ms |
| + purge every 500 ms | TinyUFO | 6,858 | 5,670 | 8 | 2.73 MB | 0 | 0 | 36.7 MiB | 80.0 / 93.8 ms |
| `include: [p]` | TinyUFO | 579,410 | 100 | 8 | 1.66 MB | 0 | 0 | 36.7 MiB | 0.83 / 1.28 ms |

Findings:

- **Origin work is bounded by admission, not by the cache.** Flood requests
  queue at the ceiling (p50 ≈ 90 ms = queue depth × render time), the origin
  never exceeds 8, and nothing is held afterwards. A unique-query flood costs
  the origin exactly what a unique-*path* flood costs, and paths are not
  cache-policy-bounded either. That is the admission controller's job.
- **Memory is bounded by the budget**: logical bytes ≤ budget, in-progress
  fill bytes ≤ one body, RSS +18–20 MiB under the flood (allocator and index
  overhead, flat over the phase). The stored key is a fixed-size hash; a long
  query does not grow an entry.
- **The hot set survives** under both policies (hit p99 ≈ 2 ms). Path purge
  during churn drops every variant of the path, hot ones included, by design.
- **Request size**: targets above about 64 KiB are refused by the `http`
  crate's URI length limit (64,000 B accepted, 120,000 B → 400), with headers
  bounded at 1 MiB / 256 by Pingora. 5,000 parameters are accepted and
  canonicalised in linear time plus a sort.
- **Coalescing-lock cardinality** is bounded by concurrent misses (ceiling
  plus queue): lock entries live only while a fill is pending.
- Canonicalisation: unique-name parameters are sorted; duplicates keep their
  order (semantics differ); values and names are not decoded in the key
  (`q=a+b` and `q=a%20b` are two entries: safe, wasteful). Equivalent
  encodings never merge.

**Decision:** keep the include-all default. Dropping parameters by default
would merge semantically different pages. Operators bound cardinality with
`cache.query.mode: include`, which the `@tanod/next` generator emits for
declared query keys. No new limit was added.

### 6.2 Query-name decoding: vulnerability (P1), fixed (`5bdc0a7`)

Include/exclude matched the raw parameter name; origins decode it as form
data. With the fixture's generated `/search` policy (`include: [q, page]`):

```text
GET /search?%71=evil   → key /search (unlisted name dropped); Next renders q=evil; stored
GET /search            → HIT: "Search results for “evil”"
GET /search?utm=1      → HIT: same
```

Policies now compare the form-decoded name. The key keeps the bytes as sent,
so `%71=evil` stays in the key as a separate entry. Uniqueness for sorting
also uses decoded names.

### 6.3 Dot segments: vulnerability (P1), fixed (`5e43ebf`)

Routing, classing and keying read the raw path; Next.js resolves `.`/`..`
(raw, or `%2e` in any case) first. Reproduced through Tanod with the fixture:

```text
GET /products/x/../../account  (Cookie: victim)  → MISS, account page for "victim", stored
GET /products/x/../../account  (Cookie: other)   → HIT, data-session="victim"
GET /_next/static/../../account                  → classed static: no origin permit
```

Ten concurrent `/_next/static/../../…` requests against a ceiling of 2 reached
an origin peak of 10. Browsers resolve dot segments before sending (RFC 3986
§5.2.4, WHATWG URL), so the disclosure needs a non-normalising client. The
admission bypass needs only `curl --path-as-is`. In protect mode such targets
now get a 400 before routing, without reaching the origin. `//` and `\` are not
refused: Next answers them with a redirect, not a render.

---

## 7. C11: the transient handoff window

**Assessment: safe; bounded; pinned by tests.** Verified under tested
assumptions.

- **Why it exists:** with streaming partial writes, Pingora releases the cache
  lock when the leader's miss handler exists. Waiters wake into an ordinary
  `lookup` with no write tag. A `TransientOnly` fill that left `temp` the
  instant it completed would make every waiter woken just before completion
  miss and render, so coalescing would fail exactly at the end of a render.
- **Who can attach:** any lookup for the same namespaced key during the render
  or within 25 ms of completion. That is followers, plus any new request that
  arrives inside the window. All of them passed request-side rules for the
  same key, and the response passed `evaluate_response` before the fill
  existed.
- **Bounded:** the timer starts at completion; attachment does not extend it.
  The reservation is released when the timer removes the fill. A reader
  already attached keeps the frozen body by reference (`Arc`), outside the
  byte accounting. It is the same detached-reader retention the security
  model documents for completed entries.
- **Purge/retirement:** every purge scope and `activate_namespace` mark
  matching fills invalidated under the lifecycle write lock; lookups filter
  invalidated fills under the temp lock. Invalidation survives the namespace
  id becoming active again. A purged handoff's timer removes only its own
  fill id, never a newer flight's.
- **Eviction:** handoffs live in `temp`, which eviction never touches; they
  are never admitted.
- **Abandoned fills** skip the handoff (`Completion::Discard`), as do retired
  or invalidated ones.

Tests: `src/cache/store_phase42_tests.rs` (new, CLOCK and TinyUFO) plus
existing `store_phase1_tests.rs` / `store_phase2_tests.rs` (abandoned
transient, late retired transient, completion/purge both orders). Mutating
the store to skip the invalidation filter fails the purge and retirement
tests; lengthening the window fails all five.

**Is 25 ms defensible?** It covers the wake-to-lookup gap on a loaded
runtime. Too short costs renders, not safety, because the window can only
widen in-flight sharing among requests for the same key that the response's
own headers already permitted. §9 Q3.

---

## 8. Tests added and results

| Test | Result on candidate | Result on `a4fe212` |
|---|---|---|
| `cache::policy` unit tests + 2 proptests (coalesce override) | pass | 4 fail |
| `config::validation` coalesce override class tests | pass | n/a (new rule) |
| `cache::key` query-name unit test + proptest | pass | fail (by construction) |
| `classifier` dot-segment proptest | pass | n/a (new function) |
| `store_phase42_tests` (5, CLOCK+TinyUFO) | pass | pass (confirm existing behaviour) |
| `bench/cache-isolation.py` CLOCK | 286/286 | 275/285 (the three vulnerabilities) |
| `bench/cache-isolation.py` TinyUFO + Next.js 16.4.0 fixture | 292/292 (before key-input checks were added) | – |
| `bench/query-cardinality.py` CLOCK, TinyUFO | all bounds hold | – |
| Full `cargo test --workspace --all-targets` | 1,238 passed (incl. TLS lib, doc) | – |

The isolation matrix covers 5 route contracts × 8 origin answers, sequential
and concurrent; for the Next.js dynamic header also cookies, multiple
cookies, `Authorization`, draft mode, Server Action and RSC. It also covers a
reload during a coalescing flight, a cancelled shared stream, query-name
spelling and dot segments. Sharing passes only where the declared contract
permits it, and the permitted paths must be observed or the run fails.

Assumptions: loopback HTTP/1.1 downstream; h2 downstream is covered by
`bench/http2.sh` for key inputs, not by this matrix. A single Tanod process
(caches and coalescing are per process). The Next.js 16.4.0 / Node 24 fixture
image. Render timings that make "concurrent" overlap (400 ms renders, second
request at 100 ms).

---

## 9. Questions requiring external reviewer judgment

1. **Should `coalesce.override_origin` lift the cookie bypass at all?** The
   alternative is that cookie-bearing requests never coalesce under it unless
   `cache.override_origin` is also set. That is safer against misdeclared
   personalised routes, but costs most of the benefit for sites where every
   visitor carries an analytics cookie.
2. **Is any signal of undeclared personalisation being missed** (C10 /
   brief §5.1)? For example a configurable list of session-cookie names that
   always bypasses, even on override routes.
3. **Is a time-based 25 ms handoff the right shape** (C11)? The alternative is
   an explicit "woken waiter" count, which would need Pingora internals.
4. **Is refusing dot-segment targets (400) preferable to normalising them**
   before routing and rewriting the upstream path?
5. **Query names:** is form-decoding (`+` → space) the right model for every
   supported origin, or should a policy match both the raw and the
   percent-only-decoded form?
6. **Persisted responses to cookie-bearing requests under `cache.override_origin`**
   are the product's headline Next.js feature. Is the explicit-class fence
   sufficient, or should the cookie lift need its own opt-in?

## Reviewer checklist

- [ ] Read §2–§3; confirm followers cannot receive bytes before `evaluate_response`.
- [ ] `evaluate_request` / `evaluate_response`: try to construct a configuration that stores a response the origin refused, without `cache.override_origin`.
- [ ] `canonical_query` / `form_decoded`: try a parameter spelling the origin reads as a listed name but the policy does not (or the reverse).
- [ ] `has_dot_segment`: try a path the fixture resolves to a different route that is not refused.
- [ ] `store.rs` lookup/finish/purge: try to attach to an invalidated or retired fill.
- [ ] Run `bench/cache-isolation.py` (both policies, with `--next-image`) and `cargo test cache:: classifier config::validation`.
- [ ] Answer §9 or mark each question as accepted risk.

---

## 10. Independent reviewer sign-off

**Not obtained.** To be completed by a reviewer who did not author the code.

| Field | Value |
|---|---|
| Reviewer | _pending_ |
| Affiliation / independence statement | _pending_ |
| Commit reviewed | _pending_ |
| Date | _pending_ |
| Scope covered (sections, files) | _pending_ |
| Findings (link) | _pending_ |
| Decision (approve / approve with conditions / reject) | _pending_ |
| Signature or verifiable link (signed commit, issue comment) | _pending_ |
