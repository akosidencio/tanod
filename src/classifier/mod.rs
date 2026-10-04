//! What kind of request is this, and may its response ever be shared?

pub mod generic;
pub mod nextjs;

use http::{HeaderMap, Method, header};

/// Every request lands in exactly one class. The class decides the *defaults*;
/// a route policy may narrow them but never widen past what safety allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Fingerprinted assets. Long-lived, freely shareable.
    Static,
    /// A rendered public document. Cacheable and coalescible when the origin
    /// (or a fenced route override) permits.
    PublicDocument,
    /// Public but parameterised — search, filters, listings. Reuse only when a
    /// route says so, because the key space is attacker-influenced.
    PublicDynamic,
    /// Per-user. Never response-cached, never coalesced. Still admission
    /// controlled, which is most of the protection anyway.
    PrivateDynamic,
    /// State-changing. Never cached, coalesced, or retried.
    Mutation,
    /// Long-lived response bodies: SSE, streamed tokens. Exempt from the origin
    /// work permit, because holding one for the life of the connection would
    /// starve every other route.
    Streaming,
    /// A protocol upgrade — WebSocket, and anything else that turns one
    /// request into a tunnel. Never cached, never coalesced, and bounded by
    /// its own ceiling rather than the render permit, for the same reason as
    /// `Streaming`: the connection outlives any render it could displace.
    Upgrade,
    /// Could not be classified. Passes through untouched.
    Unknown,
}

impl RequestClass {
    /// May a response in this class ever be stored, before any header is read?
    pub fn storable_in_principle(self) -> bool {
        matches!(
            self,
            RequestClass::Static | RequestClass::PublicDocument | RequestClass::PublicDynamic
        )
    }

    /// May concurrent equivalent requests in this class be collapsed?
    pub fn coalescible_in_principle(self) -> bool {
        matches!(
            self,
            RequestClass::PublicDocument | RequestClass::PublicDynamic
        )
    }

    /// Does a request in this class consume an origin work permit?
    ///
    /// Streaming is exempt by design: an SSE connection held for an hour would
    /// otherwise occupy a slot sized for a 200ms render.
    pub fn consumes_origin_permit(self) -> bool {
        !matches!(
            self,
            RequestClass::Static | RequestClass::Streaming | RequestClass::Upgrade
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RequestClass::Static => "static",
            RequestClass::PublicDocument => "public_document",
            RequestClass::PublicDynamic => "public_dynamic",
            RequestClass::PrivateDynamic => "private_dynamic",
            RequestClass::Mutation => "mutation",
            RequestClass::Streaming => "streaming",
            RequestClass::Upgrade => "upgrade",
            RequestClass::Unknown => "unknown",
        }
    }
}

/// The request, reduced to what classification and keying actually read.
pub struct RequestMetadata<'a> {
    pub method: &'a Method,
    pub host: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
}

/// Render a header value as text without ever losing a distinction.
///
/// `HeaderValue::to_str` fails on obs-text — the bytes 0x80..=0xFF, which
/// `HeaderValue` itself accepts. Every caller here used to write
/// `.to_str().ok()` and drop the value on failure, which is the wrong
/// direction on both counts: in a cache key, a dropped value makes a request
/// look like one that never sent the header at all, so two clients that asked
/// for different things share an entry.
///
/// `escape_ascii` is injective over byte strings, so distinct headers stay
/// distinct, and its output is printable ASCII, so it cannot contain the cache
/// key's own separators.
pub fn header_text(value: &http::HeaderValue) -> String {
    value.as_bytes().escape_ascii().to_string()
}

impl<'a> RequestMetadata<'a> {
    /// The header as text, present-and-unreadable folded into `None`.
    ///
    /// Only for values that are genuinely read as text. To ask whether a
    /// header is *there*, use [`RequestMetadata::has_header`] — a presence
    /// check written as `header(..).is_some()` answers "no" for a header that
    /// is present but not ASCII, and every such check in this crate guards
    /// something whose false answer is the unsafe one.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Is this header present at all, whatever its bytes are?
    pub fn has_header(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }

    pub fn has_authorization(&self) -> bool {
        self.headers.contains_key(header::AUTHORIZATION)
    }

    pub fn has_cookies(&self) -> bool {
        self.headers.contains_key(header::COOKIE)
    }

    /// Does any `Cookie` header carry a cookie with this name?
    ///
    /// Compared on bytes rather than on a decoded string. A single non-ASCII
    /// byte anywhere in the header — in an unrelated cookie's value — used to
    /// make the whole header unreadable and every cookie in it invisible.
    /// Since this is what detects Next.js draft mode, that turned one odd byte
    /// into a draft-mode render being cached and served publicly, while Next
    /// itself parsed the same header and honoured the cookie.
    pub fn has_cookie_named(&self, name: &str) -> bool {
        let wanted = name.as_bytes();
        self.headers.get_all(header::COOKIE).iter().any(|value| {
            value.as_bytes().split(|byte| *byte == b';').any(|pair| {
                let key = match pair.iter().position(|byte| *byte == b'=') {
                    Some(at) => &pair[..at],
                    None => pair,
                };
                key.trim_ascii() == wanted
            })
        })
    }

    pub fn is_safe_method(&self) -> bool {
        self.method == Method::GET || self.method == Method::HEAD
    }

    /// Is this request asking to leave HTTP behind?
    ///
    /// Presence of the `Upgrade` header is the signal, deliberately without
    /// also requiring `Connection: upgrade` and without reading either value.
    /// The classification this drives only ever *removes* permissions —
    /// nothing here is cacheable or coalescible — so a false positive costs a
    /// cache entry and a false negative would hand a tunnel to the microcache.
    /// HTTP/2 has no `Upgrade` mechanism at all, so an h2 request never
    /// reaches this by accident.
    pub fn is_upgrade(&self) -> bool {
        self.headers.contains_key(header::UPGRADE)
    }

    // `HEAD`, `Range` and the `If-*` preconditions deliberately have no
    // predicate here, because Tanod does not act on them itself.
    // `pingora_cache::filters::upstream::request_filter` rewrites a
    // cache-filling `HEAD` into a `GET` and strips `Range` and every `If-*`
    // header from the upstream request, so the origin returns the whole `200`
    // that a cache entry is made of, and the `206`/`304`/header-only response
    // is derived downstream from the stored entry. Re-deriving any of that
    // here would be a second implementation of the same rule, and the failure
    // mode of two implementations is that they disagree.
    //
    // What *is* worth checking is that the resulting behaviour is right, which
    // `bench/protocol.sh` does — from the outside, against the origin's own
    // render counter.
}

/// What an adapter concluded about a request.
#[derive(Debug, Clone, Default)]
pub struct RequestHints {
    pub class: Option<RequestClass>,
    /// Headers this framework requires in the cache key, because they select
    /// between genuinely different response bodies at the same URL.
    pub key_headers: Vec<&'static str>,
    /// A framework-specific reason this response must never be shared.
    pub force_bypass: Option<&'static str>,
    /// Collapse concurrent duplicates, but never store the result. For
    /// responses that are safe to share within a single instant yet have
    /// unbounded key cardinality over time.
    pub coalesce_only: bool,
}

/// Framework-specific behaviour lives behind this trait and nowhere else.
pub trait FrameworkAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn classify_request(&self, req: &RequestMetadata<'_>) -> RequestHints;
}

/// Property tests for cookie parsing and for classification under metadata
/// that was never meant to be well-formed.
///
/// `has_cookie_named` decides whether a Next.js request is in draft mode, and
/// draft mode is the difference between serving unpublished content publicly
/// and not. It parses a header format with optional whitespace, repeated
/// headers, empty segments and values that may themselves look like pairs, so
/// its edges are worth stating as properties rather than examples.
#[cfg(test)]
mod proptests {
    use super::*;
    use http::HeaderValue;
    use proptest::prelude::*;

    fn cookie_name() -> impl Strategy<Value = String> {
        prop::string::string_regex("[A-Za-z_][A-Za-z0-9_]{0,10}").unwrap()
    }

    fn cookie_value() -> impl Strategy<Value = String> {
        prop::string::string_regex("[A-Za-z0-9._-]{0,12}").unwrap()
    }

    fn request<'a>(
        method: &'a Method,
        path: &'a str,
        headers: &'a HeaderMap,
    ) -> RequestMetadata<'a> {
        RequestMetadata {
            method,
            host: "shop.example.com",
            path,
            query: None,
            headers,
        }
    }

    proptest! {
        /// Whatever else is in the header, a cookie that is present is found.
        /// A missed `__prerender_bypass` publishes a draft-mode render.
        #[test]
        fn a_present_cookie_is_always_found(
            wanted in cookie_name(),
            value in cookie_value(),
            others in prop::collection::vec((cookie_name(), cookie_value()), 0..5),
            position in 0usize..6,
            spaced in any::<bool>(),
        ) {
            let mut pairs: Vec<String> = others
                .iter()
                .filter(|(name, _)| *name != wanted)
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            let at = position.min(pairs.len());
            pairs.insert(at, format!("{wanted}={value}"));
            let joined = if spaced { pairs.join("; ") } else { pairs.join(";") };

            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_str(&joined).unwrap());
            let req = request(&Method::GET, "/p", &headers);
            prop_assert!(req.has_cookie_named(&wanted), "missed `{}` in `{}`", wanted, joined);
        }

        /// And the mirror image: a name that was never set is never reported.
        /// A false positive is a bypass on every request, which quietly
        /// disables the cache instead of corrupting it — cheaper, but still a
        /// silent policy change.
        #[test]
        fn an_absent_cookie_is_never_found(
            wanted in cookie_name(),
            others in prop::collection::vec((cookie_name(), cookie_value()), 0..5),
        ) {
            let pairs: Vec<String> = others
                .iter()
                .filter(|(name, _)| *name != wanted)
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            prop_assume!(!pairs.is_empty());
            let joined = pairs.join("; ");
            // A value that happens to contain the name must not count as the
            // name: only the part left of the first `=` is a cookie name.
            prop_assume!(!others.iter().any(|(_, value)| value.contains(&wanted)));

            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_str(&joined).unwrap());
            let req = request(&Method::GET, "/p", &headers);
            prop_assert!(!req.has_cookie_named(&wanted), "found absent `{}` in `{}`", wanted, joined);
        }

        /// A cookie split across repeated `Cookie` headers is still set. HTTP/2
        /// clients routinely send them that way.
        #[test]
        fn cookies_spread_over_repeated_headers_are_found(
            first in (cookie_name(), cookie_value()),
            second in (cookie_name(), cookie_value()),
        ) {
            prop_assume!(first.0 != second.0);
            let mut headers = HeaderMap::new();
            headers.append(header::COOKIE, HeaderValue::from_str(&format!("{}={}", first.0, first.1)).unwrap());
            headers.append(header::COOKIE, HeaderValue::from_str(&format!("{}={}", second.0, second.1)).unwrap());
            let req = request(&Method::GET, "/p", &headers);
            prop_assert!(req.has_cookie_named(&first.0));
            prop_assert!(req.has_cookie_named(&second.0));
        }

        /// Draft mode outranks every other signal. If either draft cookie is
        /// set, the Next adapter must force a bypass no matter what the rest
        /// of the request looks like — including a request that also looks
        /// like a static asset or a Server Action.
        #[test]
        fn draft_mode_always_forces_a_bypass(
            draft in prop::sample::select(vec!["__prerender_bypass", "__next_preview_data"]),
            value in cookie_value(),
            path in prop::sample::select(vec![
                "/", "/products/x", "/_next/static/chunk.js", "/favicon.ico", "/api/thing",
            ]),
            rsc in any::<bool>(),
            action in any::<bool>(),
            method in prop::sample::select(vec!["GET", "HEAD", "POST"]),
        ) {
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_str(&format!("{draft}={value}")).unwrap());
            if rsc {
                headers.insert("rsc", HeaderValue::from_static("1"));
            }
            if action {
                headers.insert("next-action", HeaderValue::from_static("abc"));
            }
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let hints = nextjs::NextJs.classify_request(&request(&method, path, &headers));
            prop_assert_eq!(hints.force_bypass, Some("next_draft_mode"));
            prop_assert_eq!(hints.class, Some(RequestClass::PrivateDynamic));
        }

        /// A Server Action is a mutation whatever URL it targets. Classifying
        /// one as a document would cache the response to a state change.
        #[test]
        fn server_actions_are_always_mutations(
            path in prop::sample::select(vec!["/", "/products/x", "/cart", "/_next/static/a.js"]),
            method in prop::sample::select(vec!["GET", "POST"]),
            action in prop::string::string_regex("[a-f0-9]{1,16}").unwrap(),
        ) {
            let mut headers = HeaderMap::new();
            headers.insert("next-action", HeaderValue::from_str(&action).unwrap());
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let hints = nextjs::NextJs.classify_request(&request(&method, path, &headers));
            prop_assert_eq!(hints.class, Some(RequestClass::Mutation));
            prop_assert_eq!(hints.force_bypass, Some("next_server_action"));
        }

        /// Classification must be total. Every input here is something a
        /// client can actually put on the wire, and a panic in the request
        /// path is a denial of service anyone can trigger.
        #[test]
        fn classification_never_panics(
            path in prop::string::string_regex("/[ -~]{0,40}").unwrap(),
            cookie in prop::string::string_regex("[ -~]{0,40}").unwrap(),
            rsc in prop::string::string_regex("[ -~]{0,20}").unwrap(),
            method in prop::sample::select(vec!["GET", "HEAD", "POST", "PUT", "DELETE", "PURGE", "GTE"]),
        ) {
            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                headers.insert(header::COOKIE, v);
            }
            if let Ok(v) = HeaderValue::from_str(&rsc) {
                headers.insert("rsc", v);
            }
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let req = request(&method, &path, &headers);
            let _ = generic::classify(&req);
            let _ = nextjs::NextJs.classify_request(&req);
            let _ = req.has_cookie_named("__prerender_bypass");
            let _ = req.is_safe_method();
        }
    }
}
