#![no_main]
//! Cookie-name lookup.
//!
//! This decides whether a Next.js request is in draft mode. A miss publishes
//! unpublished content to everybody; the header format it parses has optional
//! whitespace, empty segments, repeated headers and values that can themselves
//! look like pairs.

use arbitrary::Arbitrary;
use tanod::classifier::RequestMetadata;
use http::{HeaderMap, HeaderValue, Method, header};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    cookie_headers: Vec<&'a str>,
    wanted: &'a str,
}

fuzz_target!(|input: Input<'_>| {
    let mut headers = HeaderMap::new();
    let mut accepted: Vec<&str> = Vec::new();
    for raw in &input.cookie_headers {
        if let Ok(value) = HeaderValue::from_str(raw) {
            headers.append(header::COOKIE, value);
            accepted.push(raw);
        }
    }

    let req = RequestMetadata {
        method: &Method::GET,
        host: "shop.example.com",
        path: "/p",
        query: None,
        headers: &headers,
    };

    let found = req.has_cookie_named(input.wanted);

    // A second, deliberately naive reading of the same header, written over
    // bytes because that is what arrives on the wire. The two must agree: a
    // disagreement means one of them is wrong about draft mode.
    //
    // This target has already earned its place. Its first run found that the
    // implementation read the header with `HeaderValue::to_str`, which refuses
    // obs-text that `HeaderValue` itself accepts — so one non-ASCII byte in an
    // unrelated cookie hid every cookie in the header, including
    // `__prerender_bypass`, and an unpublished draft render was cached and
    // served publicly.
    let reference = accepted.iter().any(|raw| {
        raw.as_bytes().split(|byte| *byte == b';').any(|pair| {
            let key = match pair.iter().position(|byte| *byte == b'=') {
                Some(at) => &pair[..at],
                None => pair,
            };
            key.trim_ascii() == input.wanted.as_bytes()
        })
    });

    assert_eq!(
        found, reference,
        "disagreed on `{}` in {:?}",
        input.wanted, accepted,
    );
});
