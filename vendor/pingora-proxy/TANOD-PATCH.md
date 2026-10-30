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

A failed write is a downstream error only when the batch being written held
no failed task. Pingora delivers an upstream failure (a reset, a premature
end, a read timeout) as `HttpTask::Failed`, and writing that task returns the
upstream's error. That error propagates exactly as before, so a client still
connected gets its 502 or its visibly truncated body, and the backend is
charged with the failure. `bench/origin-concurrency.py`'s
`E-origin-fails-client-connected` scenario holds this. Errors from Tanod's own
downstream filters would also take the write path; none of them fails in
practice (`response_body_filter` cannot, `response_filter` only on a debug
header).

Pingora's queued proxy-task writes (`set_proxy_tasks_enabled`) have their own
downstream error handling, which this patch does not touch. Only Pingora's
tests enable them; Tanod does not.

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
