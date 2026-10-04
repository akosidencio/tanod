//! Next.js adapter.
//!
//! Two things here are not configurable, because getting them wrong produces a
//! wrong response rather than a slow one:
//!
//! 1. **RSC is a variant of the same URL.** With `RSC: 1` a route returns a
//!    flight payload; without it, an HTML document. Same method, same path,
//!    same query. Next also advertises these selectors in `Vary` on an ordinary
//!    HTML response, so their present *and absent* values belong in every
//!    non-static document key. Otherwise the response is either unsafe or
//!    rejected as carrying an unsupported `Vary`.
//! 2. **Draft mode must bypass.** `__prerender_bypass` and `__next_preview_data`
//!    put Next into draft mode, where the response contains unpublished
//!    content. Storing one and serving it publicly is a content leak.

use super::{FrameworkAdapter, RequestClass, RequestHints, RequestMetadata, generic};

/// Headers that select between different bodies at one URL. All four go into
/// the cache key whenever any of them is present.
pub const RSC_KEY_HEADERS: &[&str] = &[
    "rsc",
    "next-router-prefetch",
    "next-router-state-tree",
    "next-router-segment-prefetch",
    "next-url",
];

/// Cookies that put Next into draft mode.
const DRAFT_COOKIES: &[&str] = &["__prerender_bypass", "__next_preview_data"];

pub struct NextJs;

impl NextJs {
    fn is_static_asset(path: &str) -> bool {
        path.starts_with("/_next/static/") || path == "/favicon.ico"
    }

    fn is_rsc(req: &RequestMetadata<'_>) -> bool {
        // Presence, not content. `header(..).is_some()` would answer "no" for
        // `RSC: <obs-text>`, classifying a flight payload as a document and
        // keying it as one.
        req.has_header("rsc")
    }

    fn is_prefetch(req: &RequestMetadata<'_>) -> bool {
        req.has_header("next-router-prefetch") || req.has_header("next-router-segment-prefetch")
    }

    fn is_server_action(req: &RequestMetadata<'_>) -> bool {
        // A missed Server Action is classified as a document and becomes
        // cacheable, so this check may never depend on the id being readable.
        req.has_header("next-action")
    }

    fn in_draft_mode(req: &RequestMetadata<'_>) -> bool {
        DRAFT_COOKIES.iter().any(|c| req.has_cookie_named(c))
    }
}

impl FrameworkAdapter for NextJs {
    fn name(&self) -> &'static str {
        "nextjs"
    }

    fn classify_request(&self, req: &RequestMetadata<'_>) -> RequestHints {
        // An upgrade outranks even draft mode, because it is not a question
        // about *this* framework at all: the request is asking to stop
        // speaking HTTP. Every branch below assumes a request/response
        // exchange, and `next dev`'s hot-reload socket is exactly the traffic
        // that would otherwise be classified by path and handed to the cache.
        if req.is_upgrade() {
            return RequestHints {
                class: Some(RequestClass::Upgrade),
                force_bypass: Some("protocol_upgrade"),
                ..Default::default()
            };
        }

        // Draft mode outranks everything else, including a route that declares
        // itself public: the body contains unpublished content.
        if Self::in_draft_mode(req) {
            return RequestHints {
                class: Some(RequestClass::PrivateDynamic),
                force_bypass: Some("next_draft_mode"),
                ..Default::default()
            };
        }

        // A Server Action is a POST to the page's own URL. It must not be
        // confused with a document request for that same URL.
        if Self::is_server_action(req) {
            return RequestHints {
                class: Some(RequestClass::Mutation),
                force_bypass: Some("next_server_action"),
                ..Default::default()
            };
        }

        if Self::is_static_asset(req.path) {
            return RequestHints {
                class: Some(RequestClass::Static),
                ..Default::default()
            };
        }

        if Self::is_rsc(req) {
            let base = generic::classify(req);
            // A prefetch payload is keyed on the router state tree, which
            // encodes the whole client route state — near-unbounded
            // cardinality. Worth collapsing a burst of them, never worth
            // storing.
            let coalesce_only = Self::is_prefetch(req);
            return RequestHints {
                class: Some(base),
                key_headers: RSC_KEY_HEADERS.to_vec(),
                coalesce_only,
                force_bypass: None,
            };
        }

        RequestHints {
            class: Some(generic::classify(req)),
            // Next emits `Vary` for these selectors on normal HTML too. Keying
            // their absence is what keeps an HTML document separate from a
            // later RSC or prefetch request for the same URL.
            key_headers: RSC_KEY_HEADERS.to_vec(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method, header};

    fn get<'a>(path: &'a str, h: &'a HeaderMap) -> RequestMetadata<'a> {
        RequestMetadata {
            method: &Method::GET,
            host: "shop.example.com",
            path,
            query: None,
            headers: h,
        }
    }

    #[test]
    fn next_static_assets_are_static() {
        let h = HeaderMap::new();
        let hints = NextJs.classify_request(&get("/_next/static/chunks/main-abc123.js", &h));
        assert_eq!(hints.class, Some(RequestClass::Static));
    }

    #[test]
    fn rsc_requests_put_the_variant_headers_in_the_key() {
        // Without this, the flight payload and the HTML document collide.
        let mut h = HeaderMap::new();
        h.insert("rsc", HeaderValue::from_static("1"));
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(hints.key_headers.contains(&"rsc"));
        assert!(hints.key_headers.contains(&"next-router-state-tree"));
    }

    #[test]
    fn a_plain_document_keys_absent_rsc_variant_headers() {
        let h = HeaderMap::new();
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert_eq!(hints.key_headers, RSC_KEY_HEADERS);
        assert_eq!(hints.class, Some(RequestClass::PublicDocument));
    }

    #[test]
    fn prefetch_rsc_is_coalesced_but_never_stored() {
        let mut h = HeaderMap::new();
        h.insert("rsc", HeaderValue::from_static("1"));
        h.insert("next-router-prefetch", HeaderValue::from_static("1"));
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(
            hints.coalesce_only,
            "state-tree cardinality makes storing these pointless"
        );
    }

    #[test]
    fn segment_prefetch_is_also_coalesced_but_never_stored() {
        let mut h = HeaderMap::new();
        h.insert("rsc", HeaderValue::from_static("1"));
        h.insert(
            "next-router-segment-prefetch",
            HeaderValue::from_static("/products/[slug]/page"),
        );
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(hints.coalesce_only);
        assert!(hints.key_headers.contains(&"next-router-segment-prefetch"));
    }

    #[test]
    fn server_actions_bypass_everything() {
        let mut h = HeaderMap::new();
        h.insert("next-action", HeaderValue::from_static("abc123"));
        let req = RequestMetadata {
            method: &Method::POST,
            host: "shop.example.com",
            path: "/checkout",
            query: None,
            headers: &h,
        };
        let hints = NextJs.classify_request(&req);
        assert_eq!(hints.class, Some(RequestClass::Mutation));
        assert_eq!(hints.force_bypass, Some("next_server_action"));
    }

    #[test]
    fn draft_mode_bypasses_even_on_a_public_looking_path() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=1; __prerender_bypass=xyz"),
        );
        let hints = NextJs.classify_request(&get("/blog/unpublished-post", &h));
        assert_eq!(hints.force_bypass, Some("next_draft_mode"));
        assert_eq!(hints.class, Some(RequestClass::PrivateDynamic));
    }

    #[test]
    fn draft_cookie_match_is_exact_not_substring() {
        // `not__prerender_bypass_x` must not trip the draft-mode check.
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("not__prerender_bypass_x=1"),
        );
        let hints = NextJs.classify_request(&get("/blog/post", &h));
        assert_eq!(hints.force_bypass, None);
    }

    /// Found by the `cookies` fuzz target.
    ///
    /// `HeaderValue` accepts obs-text (0x80..=0xFF) but `to_str` refuses it, so
    /// reading the cookie header as a string dropped the *entire* header when a
    /// single unrelated cookie carried one odd byte — and with it the draft-mode
    /// cookie. Next.js parses the same header from bytes and honours the cookie,
    /// so Tanod cached an unpublished render and served it publicly.
    #[test]
    fn a_non_ascii_byte_elsewhere_in_the_header_cannot_hide_the_draft_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_bytes(b"locale=\xd0\xa5; __prerender_bypass=abc").unwrap(),
        );
        let hints = NextJs.classify_request(&get("/preview", &headers));
        assert_eq!(hints.force_bypass, Some("next_draft_mode"));
    }

    /// The same failure reached through a presence check. A prefetch payload
    /// is keyed on the entire client route state, so it is collapsed but never
    /// stored; a prefetch header whose bytes `to_str` cannot read used to make
    /// the request look like an ordinary one, and an unbounded key space
    /// became storable.
    #[test]
    fn an_unreadable_prefetch_header_is_still_a_prefetch() {
        let mut headers = HeaderMap::new();
        headers.insert("rsc", HeaderValue::from_static("1"));
        headers.insert(
            "next-router-prefetch",
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        let hints = NextJs.classify_request(&get("/products/iphone", &headers));
        assert!(
            hints.coalesce_only,
            "a prefetch with an unreadable header became storable"
        );
    }

    /// And the one with the worst consequence: an unreadable action id must
    /// still classify as a mutation, or a state change becomes cacheable.
    #[test]
    fn an_unreadable_action_id_is_still_a_mutation() {
        let mut headers = HeaderMap::new();
        headers.insert("next-action", HeaderValue::from_bytes(b"\x80\x81").unwrap());
        let post = Method::POST;
        let hints = NextJs.classify_request(&RequestMetadata {
            method: &post,
            host: "shop.example.com",
            path: "/cart",
            query: None,
            headers: &headers,
        });
        assert_eq!(hints.class, Some(RequestClass::Mutation));
        assert_eq!(hints.force_bypass, Some("next_server_action"));
    }
}
