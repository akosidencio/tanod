# pingora-proxy 0.9.0, patched for Tanod

This directory is the published `pingora-proxy` 0.9.0 crate (crates.io
checksum `2467265ad39b56230b56149f1c03ca510f46c4bf4a25d6d4b7345e0f06cc46e7`)
with one change, recorded exactly in [`tanod.patch`](tanod.patch).
`Cargo.toml` and `fuzz/Cargo.toml` select it with `[patch.crates-io]`.

Verify that nothing else differs:

```sh
scripts/check-vendored-pingora.sh
```

## Why

Pingora ends an upstream exchange the moment its downstream fails, unless
the response is filling the cache. For an uncached response, or one the cache
declined under memory pressure, a client disconnect closes the origin
connection, the request finishes, and Tanod's origin permit is released while
the origin may still be rendering. A queued request is then admitted beside it.
The origin's concurrency exceeds Tanod's ceiling by up to the number of
disconnects in flight. `bench/origin-concurrency.py` reproduces it in seconds:
a disconnect storm against a four-unit ceiling reached thirteen
concurrent renders before this change.

## What changes

One hook on `ProxyHttp`, defaulting to the existing behaviour:

```rust
fn upstream_after_downstream_error(&self, session: &Session, ctx: &mut Self::CTX)
    -> Option<std::time::Instant>
```

When a downstream read or write fails outside a cache fill, and the request
body was completely read, and the session was not upgraded, the HTTP/1 and
HTTP/2 upstream loops ask the hook. `None` abandons the exchange as before.
`Some(deadline)` applies what Pingora already does for a cache fill: the
downstream is marked errored, the upstream response keeps being read and
passes through the upstream filters, its tasks are discarded instead of
written, and the exchange fails with `ReadTimedout` if it has not ended by
the deadline.

Not changed: cache-fill behaviour, custom upstream sessions, upgrades,
requests whose body was still arriving, connection reuse rules, and every
code path where the hook returns `None`.

## Updating Pingora

Re-apply `tanod.patch` to the new release, regenerate it against the
published crate, update the version and checksum in
`scripts/check-vendored-pingora.sh` and this file, and run
`bench/origin-concurrency.py`. Drop the patch if Pingora gains an equivalent
hook.

`cargo audit` reads advisories for registry packages; a path package is not
matched against the RustSec database. Check advisories for `pingora-proxy`
by hand when updating.
