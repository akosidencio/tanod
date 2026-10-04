#![no_main]
//! Cache-key construction under arbitrary request metadata.
//!
//! The invariant is the one the whole cache rests on: `pingora-cache` hashes
//! the canonical string, so two keys that are structurally distinct must never
//! render to the same string. A counterexample here is one visitor's page
//! stored under another visitor's key.
//!
//! The property tests assert this over generated components; the fuzzer
//! attacks it from the other side, driving the real `KeyBuilder` with bytes a
//! client could put on the wire.

use arbitrary::Arbitrary;
use tanod::cache::KeyBuilder;
use tanod::classifier::RequestMetadata;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Request<'a> {
    scheme: &'a str,
    host: &'a str,
    method: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    headers: Vec<(&'a str, &'a str)>,
    deployment: Option<&'a str>,
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    left: Request<'a>,
    right: Request<'a>,
}

fn build(req: &Request<'_>) -> Option<tanod::cache::CacheKey> {
    let method = Method::from_bytes(req.method.as_bytes()).ok()?;
    let mut headers = HeaderMap::new();
    let mut variant = Vec::new();
    for (name, value) in &req.headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        variant.push(name.as_str().to_string());
        headers.append(name, value);
    }
    Some(
        KeyBuilder {
            scheme: req.scheme,
            query_policy: None,
            variant_headers: &variant,
            deployment: req.deployment,
        }
        .build(&RequestMetadata {
            method: &method,
            host: req.host,
            path: req.path,
            query: req.query,
            headers: &headers,
        }),
    )
}

fuzz_target!(|input: Input<'_>| {
    let (Some(a), Some(b)) = (build(&input.left), build(&input.right)) else {
        return;
    };

    // Determinism: the same request must always produce the same entry, or a
    // hit becomes a coin flip.
    assert_eq!(a.canonical_string(), a.canonical_string());
    assert_eq!(a.fingerprint(), a.fingerprint());

    // Injectivity: structural equality and rendered equality must agree.
    assert_eq!(
        a == b,
        a.canonical_string() == b.canonical_string(),
        "canonical string disagreed with structural equality:\n  a = {a:?}\n  b = {b:?}",
    );

    // The fingerprint is for logs only, so collisions are acceptable there —
    // but two keys that ARE equal must never fingerprint apart, or the same
    // entry appears under two identities in a trace.
    if a == b {
        assert_eq!(a.fingerprint(), b.fingerprint());
    }
});
