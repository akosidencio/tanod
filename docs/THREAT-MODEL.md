# Tanod threat model

Tanod is an overload governor for expensive SSR and dynamic origin workloads,
not a general Next.js performance accelerator. This model therefore treats
unbounded or amplified origin work as a primary availability threat alongside
the confidentiality risks introduced by safe response reuse.

**Status: first edition, written 2026-08-27 alongside roadmap phase 1. Not
independently reviewed.** Tanod has never run in production and has had no
third-party security review. This document is the author's own analysis; read
it as a statement of what the design intends and where the author believes it
is weak, not as an assurance that it holds.

The point of writing it down is that a cache is a component whose failures are
silent by construction. A cache that serves the wrong body serves it quickly,
with a 200, and looks healthy on every dashboard. So the useful question is not
"is Tanod secure" but "what specifically would have to go wrong, and what
stops each one".

---

## 1. What is being protected

Three assets, in order of how bad it is to lose them.

| # | Asset | What losing it looks like |
| --- | --- | --- |
| A1 | **Response confidentiality** — one user's rendered page never reaches another | A logged-in user sees someone else's account page, cart, or draft content |
| A2 | **Origin availability** — the origin is not driven past its render capacity | The origin's event loop queues, health checks time out, the orchestrator restarts pods, survivors inherit the load |
| A3 | **Proxy availability** — Tanod itself keeps serving | Everything behind Tanod is down, including the parts that would have survived without it |

A1 outranks A2 absolutely. Every mechanism in Tanod that improves A2 —
caching, coalescing — is a mechanism that shares a response, and every place
those two pull against each other is resolved in favour of A1. That is why
`Set-Cookie` is an unconditional refusal that no configuration reaches, and why
a route-level override of the origin's cache directives requires an explicit
`class:` and an explicit TTL ceiling before it is accepted.

---

## 2. Who the adversaries are

| # | Adversary | Capability assumed |
| --- | --- | --- |
| T1 | **An anonymous internet client** | Sends any syntactically valid HTTP over any supported version. Chooses every header, path, query, cookie and body. Opens many connections. Reads responses arbitrarily slowly, or not at all. |
| T2 | **An authenticated user** | Everything T1 has, plus a valid session for their own account. Wants somebody else's response. |
| T3 | **A hostile or compromised origin** | Returns any status, headers and body, including framing that contradicts itself. In scope because SSR origins render third-party content and because a compromised origin should not be able to escalate into a cross-user leak through the cache. |
| T4 | **A network position between Tanod and the origin** | Reads and modifies the origin connection. Only in scope where `origin.tls` is configured, and the honest answer without it is that Tanod offers nothing here. |
| T5 | **An operator writing a config** | Not malicious. In scope because a configuration mistake in *this* component is a data leak, and because "the config was wrong" is not a defence a user cares about. |

Explicitly **out of scope**: an attacker with local code execution or memory
access on the Tanod host; an attacker who can modify Tanod's binary or
config file; side channels (timing, cache occupancy) that reveal *whether* a
URL is cached rather than its content; and denial of service by raw bandwidth
exhaustion, which is a layer below this.

---

## 3. Trust boundaries

```
                     ┌──────────────────────────────────────────┐
    T1, T2           │  Tanod                                 │        T3
  ────────────▶ (B1) │                                          │ (B3) ◀────────
   client            │   classify ─▶ key ─▶ share? ─▶ admit     │      origin
   connection        │                  │                        │
                     │                  ▼                        │
                     │            shared cache  ◀── (B2) ────────┤
                     └──────────────────────────────────────────┘
                                        ▲
                                       T5
                                  config file
```

* **B1 — the client connection.** Everything crossing it is attacker-chosen.
  The only unforgeable facts are the peer address and whether the connection is
  TLS; both are read from the socket, never from a header.
* **B2 — the cache.** The single place where data from one request can reach
  another. Everything that decides what may cross it is in `cache::key` and
  `cache::policy`, deliberately kept as pure logic beneath the proxy so it can
  be tested without a runtime.
* **B3 — the origin response.** Attacker-influenced under T3, and the input to
  the shareability decision that guards B2.

---

## 4. Threats, and what answers each

### 4.1 Cross-user response disclosure (A1)

| Threat | Mechanism |
| --- | --- |
| A response addressed to one user is stored and served to another | `Set-Cookie` on a response is an unconditional `NotShareable`. No route override reaches it. `evaluate_response` in `cache/policy.rs` |
| A credentialed request's response is stored | `Authorization` bypasses reuse before the origin is touched; any `Cookie` on the request makes the class `private_dynamic` unless a route explicitly overrode it |
| The origin says it varies on something the key does not carry, and the entry is served to a request that wanted a different variant | An origin `Vary` naming any header outside the cache key is `NotShareable`. `unsupported_vary` |
| Two structurally different requests produce one cache key | The key is built structurally and rendered with a **length-prefixed** encoding, so injectivity is a property of the encoding rather than of `http`'s input validation. Asserted by a property test that fails against the old separator-only encoding |
| A header value the key needs is dropped because it is not ASCII | Header values reach the key through `escape_ascii`, which is injective and printable. Presence checks use `contains_key` and never `header(..).is_some()`. This was a real bug: one obs-text byte in an unrelated cookie hid `__prerender_bypass` and published a draft-mode render |
| Over HTTP/2 the authority is missing from the key, merging every virtual host | `request_host` reads the `Host` header **or** the URI authority. This was a real bug, found while writing `bench/http2.sh`, and reproduces the moment `server.h2c` or `server.tls` is enabled |
| A `206` or `304` is stored under the full document's key | Only `200, 203, 204, 301, 308, 404, 410` are storable; Pingora strips `Range` and `If-*` from a cache-filling upstream request so the origin returns the whole document |
| A `HEAD` fills the cache with a bodyless entry | Pingora rewrites a cache-filling `HEAD` to `GET` upstream and drops the body downstream. Asserted in `bench/protocol.sh` |
| Draft-mode / preview content is published | `__prerender_bypass` and `__next_preview_data` force a bypass ahead of every other rule in the Next.js adapter, matched on bytes |

**Residual risk.** The absolute rules are absolute only against the response
signals Tanod can see. An origin that personalises a response *without*
`Set-Cookie`, `Cache-Control: private` or a `Vary` — personalising on a header
it did not declare — is indistinguishable from a public one. `cache.vary` and
route classes exist for that case and are the operator's responsibility (T5).
This is the largest unmitigated confidentiality risk in the design.

### 4.2 Origin-work amplification (A2)

The inversion that matters: Tanod sits in front of the origin to *reduce*
origin work, so any input a client controls that multiplies cache keys turns
Tanod into an amplifier.

| Threat | Mechanism |
| --- | --- |
| `?cachebust=<random>` mints a key per request | `cache.query.mode: include` drops unlisted parameters from the key. Validation refuses `mode` with an empty `keys` |
| A forged `X-Forwarded-Proto` mints a key per invented scheme string | Forwarded headers are read only from a peer inside `server.trusted_proxies.from`, which is empty by default; and the scheme is normalised to exactly `http` or `https` for everyone, trusted or not. `bench/forwarded.sh` |
| A forged `X-Forwarded-For` mints keys, or forges identity in the origin's logs and rate limits | Same trust gate. `X-Forwarded-For` is **replaced**, never appended to, and `Forwarded` is removed outright |
| Unbounded router-state cardinality on Next.js prefetches | Prefetches are coalesce-only: collapsed within one flight, never stored |
| Query parameter ordering doubles entries | The canonical query is sorted |
| A herd of misses all reach the origin | Cache locks collapse concurrent equivalent requests, including on streaming responses via the partial-write path |
| An uncacheable leader releases its waiters as an unbounded herd | Pingora's documented `LockStatus::GiveUp`. Safe — nothing is shared — but the herd is bounded by admission control, not by the lock |
| Everything above fails and the origin is flooded anyway | Per-route and global admission with a bounded queue, a queue deadline, and a defined overload response. This is the mechanism that works when nothing is cacheable |

### 4.3 Capacity leaks (A2, A3)

A permit that is taken and never returned tightens Tanod's own ceiling until
it admits nothing. That failure presents as an overloaded origin and is not
one, which makes it worth enumerating separately.

| Threat | Mechanism |
| --- | --- |
| A slow reader holds a render permit after the origin has finished | **The response spool** (`proxy/spool.rs`). Bounded per response and globally; degrades to the previous behaviour on overflow. `bench/spool.sh` measures both directions |
| A client hangs up mid-render and the permit is lost | Released in `logging`, which runs on every path including errors. Asserted in `bench/protocol.sh` with six abandoned requests against a ceiling of one |
| A WebSocket holds a render permit for the life of the socket | Upgrades take a separate ceiling (`upgrade.max_concurrent`) and never a render permit. Off by default. `bench/websocket.sh` renders a page with every socket held open against a render ceiling of one |
| An SSE stream holds a permit for an hour | `class: streaming` is exempt from the render permit by design |
| Spooled bytes accumulate across many slow readers | `spool.max_memory` bounds every in-flight spool at once; the reservation is held until the request ends, not released at flush, because the bytes stay resident in the write buffer exactly while a slow client is reading |
| The cache grows without bound | `cache.max_memory` is enforced during a fill, not only at admission |

### 4.4 Hostile or broken origin (T3)

| Threat | Mechanism |
| --- | --- |
| A truncated body is promoted to a complete cache entry and replayed for the full TTL | The miss handler is only promoted on `finish`, which Pingora calls only at a genuine end of stream; a dropped handler removes its temporary entry and returns its bytes. Asserted for both `Content-Length` truncation and unterminated chunked framing in `bench/protocol.sh` |
| An oversized body exhausts memory during a fill | `cache.max_body_size` is tracked as the body arrives, not only from `Content-Length` |
| The origin sets Tanod's own internal marker header | `TRANSIENT_HEADER` is removed from every stored and forwarded response before Tanod sets it |
| A slow or hanging origin holds a permit forever | `timeouts.origin` is re-checked in the response and body filters, not only as a socket timeout |

**Residual risk.** Request smuggling between Tanod and the origin is
Pingora's HTTP/1.1 parser to defend, not Tanod's. Tanod adds no
request-line or header rewriting that could reintroduce it, but it also does not
independently validate framing. This has not been tested adversarially.

### 4.5 Proxy availability (A3)

| Threat | Mechanism |
| --- | --- |
| A panic in the request path | Classification, cookie parsing, query canonicalisation, `Vary` evaluation, `Cache-Control` parsing and forwarded-header resolution all have proptests asserting totality, and seven fuzz targets in CI. `bench/adversarial.sh` fails if the log contains a panic at all |
| Metric cardinality explosion from client-controlled labels | Only config-derived labels — route id, class, limiter name, upstream — are ever used. A unit test walks the whole registry and fails on any other label name |
| An unbounded queue turns origin overload into proxy overload | `queue.max` is required, and validation refuses a queue with no deadline |
| Cache-key or log injection through a crafted path | The access log escapes per RFC 8259; a test asserts a crafted path cannot forge a field. The key encoding is length-prefixed |

### 4.6 Operator error (T5)

The design position is that a config which parses must mean what it says.

* Every struct is `deny_unknown_fields`, so a typo is a startup failure rather
  than a silently disabled protection.
* **Options that are accepted but not implemented are rejected at startup.**
  Currently: `cache.respect_origin: false`, `deployment.id_header`,
  `origin.tls.ca` (Pingora 0.8's rustls connector never reads the per-peer CA
  store), `server.tls` and `origin.tls` in a binary built without the `tls`
  feature, and `origin.http_version: auto` without TLS.
* Combinations that would leak are refused: `override_origin` on a private
  class, `override_origin` without an explicit class or TTL ceiling,
  `cache.vary` on `Cookie`/`Authorization`/`User-Agent`/`*`, a coalescing wait
  shorter than the origin timeout, spooling a streaming route.
* `tanod check` prints the settings that trade a safety property for
  convenience: an empty trusted-proxy list, `verify_cert: false`, upgrades
  enabled, spooling enabled.

**Residual risk.** Nothing prevents an operator from putting
`class: public_ssr` on a genuinely personalised route whose origin does not
signal it. That is the T5 mirror of the 4.1 residual risk and has the same
answer: it cannot be detected from the outside.

---

## 5. What is deliberately not defended

Stated plainly, because a threat model that omits them implies coverage that
does not exist.

1. **Cache occupancy side channels.** An attacker can learn whether a URL is
   currently cached by timing. `debug_headers` is off by default for the same
   reason, but timing remains.
2. **Bandwidth-level denial of service.** Tanod bounds origin *work*. It does
   not bound bytes, connections per source, or request rate. There is no
   per-client rate limiting of any kind.
3. **Request smuggling.** Inherited from Pingora, untested here.
4. **Multi-instance capacity.** Two Tanod replicas each enforce their own
   ceiling, so the effective origin limit is the sum. Roadmap phase 5.
5. **Purge and invalidation.** There is no way to evict an entry that turns out
   to be wrong except by waiting for its TTL. Roadmap phase 4.
6. **Compromise of the Tanod host.** Out of scope entirely.

---

## 6. What would change this document's conclusions

The honest summary is that the mechanisms are argued and tested, and the
argument has not been independently checked. Three things would move it:

1. **An independent review of cache-key construction and response
   shareability** — the two components where a mistake is a confidentiality
   failure. `docs/CACHE-KEY-REVIEW.md` is written for a reviewer to work
   through and states the specific claims to attack. **Not yet obtained.**
2. **Production traffic.** Every number in this repository comes from a
   deterministic fixture and a local Next.js container. Real traffic contains
   header shapes nobody thought to generate.
3. **A longer adversarial campaign.** `bench/adversarial.sh` runs for twenty
   seconds in CI. That is a smoke test. A memory leak, a slow permit leak, or
   an eviction pathology needs hours.

---

## 7. Reporting a problem

There is no security contact process yet, because there are no users to have
one for. Open a GitHub issue. If that is the wrong venue for what you have
found, say so in the issue without the details and a private channel will be
arranged.
