#![no_main]
//! Metadata classification must never authorize reuse of unsafe requests.
use arbitrary::Arbitrary;
use http::{HeaderMap, HeaderValue, Method};
use libfuzzer_sys::fuzz_target;
use tanod::cache::policy::{Disposition, evaluate_request};
use tanod::classifier::{FrameworkAdapter, RequestClass, RequestMetadata, nextjs::NextJs};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    method: u8,
    path: &'a str,
    cookie: &'a [u8],
    authorization: &'a [u8],
    action: bool,
    draft: bool,
}

fuzz_target!(|input: Input<'_>| {
    let method = match input.method % 4 {
        0 => Method::GET,
        1 => Method::HEAD,
        2 => Method::POST,
        _ => Method::PUT,
    };
    let mut headers = HeaderMap::new();
    for (name, raw) in [
        ("cookie", input.cookie),
        ("authorization", input.authorization),
    ] {
        if !raw.is_empty()
            && let Ok(value) = HeaderValue::from_bytes(raw)
        {
            headers.insert(name, value);
        }
    }
    if input.action {
        headers.insert("next-action", HeaderValue::from_static("fixture"));
    }
    if input.draft {
        headers.append(
            "cookie",
            HeaderValue::from_static("__prerender_bypass=fixture"),
        );
    }
    let request = RequestMetadata {
        method: &method,
        host: "shop.example",
        path: input.path,
        query: None,
        headers: &headers,
    };
    let hints = NextJs.classify_request(&request);
    let class = hints.class.unwrap_or(RequestClass::PublicDocument);
    let verdict = evaluate_request(&request, class, hints.force_bypass, None, false, true, true);
    if !request.is_safe_method()
        || request.has_authorization()
        || request.has_cookies()
        || input.action
        || input.draft
    {
        assert!(
            matches!(verdict, Disposition::Bypass(_)),
            "unsafe reuse: {input:?}"
        );
    }
});
