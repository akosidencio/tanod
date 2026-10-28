//! Cache key construction.
//!
//! The key is built *structurally*: a struct of components with derived `Eq`
//! and `Hash`, so two distinct pages cannot become one by colliding under a
//! short hash. That guarantee ends at the boundary with `pingora-cache`, which
//! takes a string primary and blake2-hashes it itself — so
//! [`CacheKey::canonical_string`] is where the structural key has to be
//! rendered without losing the distinctions it was built to keep. A separate
//! short fingerprint is derived for identifying one key in a trace, and is
//! never used for lookup — nor, deliberately, written to the access log; see
//! [`CacheKey::fingerprint`].

use crate::classifier::RequestMetadata;
use crate::config::schema::{QueryMode, QueryPolicy};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Carried in Pingora extensions for admission, and encoded in the primary
/// key for both completed-cache lookup and Pingora's coalescing lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheNamespace(pub u64);

impl CacheNamespace {
    /// The Pingora key for `key` in this namespace: exactly
    /// `key(key.canonical_string(), path)`, rendered into one buffer instead
    /// of two.
    pub(crate) fn key_for(self, key: &CacheKey, path: &str) -> pingora_cache::key::CacheKey {
        use std::fmt::Write as _;
        let mut primary = String::with_capacity(192);
        // Writing to a `String` cannot fail.
        let _ = write!(primary, "tanod-policy-v1:{}:", self.0);
        key.write_canonical(&mut primary);
        let mut key = pingora_cache::key::CacheKey::new(primary, path);
        key.extensions.insert(self);
        key
    }

    /// The same key from an already rendered canonical string. Tests use it
    /// to state the encoding independently of [`key_for`](Self::key_for).
    #[cfg(test)]
    pub(crate) fn key(self, canonical: String, path: &str) -> pingora_cache::key::CacheKey {
        // The fixed marker, decimal number and colon have unambiguous bounds.
        let mut key = pingora_cache::key::CacheKey::new(
            format!("tanod-policy-v1:{}:{canonical}", self.0),
            path,
        );
        key.extensions.insert(self);
        key
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    scheme: String,
    host: String,
    method: String,
    path: String,
    /// Canonical query: filtered by policy, then sorted so that `?a=1&b=2` and
    /// `?b=2&a=1` are one entry rather than two renders.
    query: String,
    /// Sorted `(name, value)` pairs for every header that selects a variant:
    /// configured `Vary`, the framework's own variant headers, and the
    /// negotiated content encoding.
    variant: Vec<(String, String)>,
    deployment: Option<String>,
}

impl CacheKey {
    /// A stable short id for one key. Never used for lookup.
    ///
    /// Deliberately not emitted by [`crate::telemetry::logging`]: that module
    /// omits the query string because it routinely carries session tokens and
    /// signed URLs, and this digest is taken over a tuple that *includes* the
    /// query, under `DefaultHasher`'s fixed keys. Publishing it would hand
    /// anyone holding the log a cheap way to confirm a guessed URL, which is
    /// most of what leaving the query out was protecting.
    ///
    /// It is `pub` for the `cache_key` fuzz target, which asserts the property
    /// that matters here — two keys that compare equal must never fingerprint
    /// apart — from outside the crate. Wire it into a trace only where the
    /// span is already narrower than the access log.
    pub fn fingerprint(&self) -> String {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Render the key as one canonical string.
    ///
    /// Pingora's `CacheKey` takes a string primary and hashes it itself, so
    /// this is the bridge between our structural key and its storage key. Two
    /// distinct structural keys rendering to one string is a cross-user
    /// response leak, so the encoding has to be injective — `host=a.com
    /// path=/b` and `host=a.com/b path=` must not collide.
    ///
    /// Each component is therefore length-prefixed, not merely separated. A
    /// separator alone is injective only while no component can contain that
    /// separator byte, which is true today (`http` rejects control characters
    /// in URIs and header values) but is a property of a dependency rather
    /// than of this function. A length prefix makes injectivity hold for any
    /// input at all, which is what the property test asserts.
    pub fn canonical_string(&self) -> String {
        let mut out = String::with_capacity(160);
        self.write_canonical(&mut out);
        out
    }

    /// [`canonical_string`](Self::canonical_string), appended to `out`.
    fn write_canonical(&self, out: &mut String) {
        use std::fmt::Write as _;
        let mut push = |part: &str| {
            // The separators are kept as well: they cost nothing and keep a
            // fingerprinted key readable when one turns up in a log. The
            // length is formatted straight into the key, not through a
            // temporary string per component.
            let _ = write!(out, "{}", part.len());
            out.push('\u{1e}');
            out.push_str(part);
            out.push('\u{1f}');
        };
        for part in [
            self.scheme.as_str(),
            self.host.as_str(),
            self.method.as_str(),
            self.path.as_str(),
            self.query.as_str(),
        ] {
            push(part);
        }
        // Absence and emptiness are different states, and folding them
        // together with `unwrap_or("")` made a deployment id of `""` render
        // exactly like no deployment id at all. Found by the `cache_key` fuzz
        // target.
        match &self.deployment {
            Some(id) => {
                push("d");
                push(id);
            }
            None => push("-"),
        }
        // The variant count is part of the encoding too: without it, one key
        // with variants [a] and another with [a, b] where b renders empty
        // could otherwise agree.
        push(&self.variant.len().to_string());
        for (name, value) in &self.variant {
            push(name);
            push(value);
        }
    }
}

/// Everything that goes into a key, assembled by the caller from route policy
/// and adapter hints.
pub struct KeyBuilder<'a> {
    pub scheme: &'a str,
    /// The public host a trusted proxy reported, already validated by
    /// [`crate::net::forwarded`]. `None` leaves the key exactly as it was
    /// before host forwarding existed.
    pub forwarded_host: Option<&'a str>,
    pub query_policy: Option<&'a QueryPolicy>,
    /// Header names from route `cache.vary` plus the framework adapter's
    /// mandatory variant headers.
    pub variant_headers: &'a [String],
    pub deployment: Option<&'a str>,
}

impl<'a> KeyBuilder<'a> {
    pub fn build(&self, req: &RequestMetadata<'_>) -> CacheKey {
        let mut variant: Vec<(String, String)> = self
            .variant_headers
            .iter()
            // Header lookup is case-insensitive, so the lowercased name is
            // only allocated for a header the request actually carries; most
            // requests carry none of the framework's variant headers.
            .filter_map(|name| header_values(req, name).map(|v| (name.to_ascii_lowercase(), v)))
            .collect();

        // Content coding is part of the identity of a stored body. Omit it and
        // a brotli entry eventually reaches a client that only reads gzip.
        variant.push(("~encoding".into(), normalize_accept_encoding(req)));
        // The origin sees this as `X-Forwarded-Host` and builds URLs from it,
        // so it selects a representation exactly as the routing host does.
        if let Some(host) = self.forwarded_host {
            variant.push(("~forwarded-host".into(), normalize_host(host, self.scheme)));
        }
        variant.sort();

        CacheKey {
            scheme: self.scheme.to_ascii_lowercase(),
            host: normalize_host(req.host, self.scheme),
            method: req.method.as_str().to_ascii_uppercase(),
            path: req.path.to_string(),
            query: canonical_query(req.query.unwrap_or(""), self.query_policy),
            variant,
            deployment: self.deployment.map(str::to_string),
        }
    }
}

fn normalize_host(host: &str, scheme: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(":80"),
        "https" => Some(":443"),
        _ => None,
    };
    default_port
        .and_then(|port| host.strip_suffix(port))
        .unwrap_or(&host)
        .to_string()
}

/// Collapse `Accept-Encoding` to the coding we would actually negotiate.
///
/// Bounded to three values on purpose: keying on the raw header would mint a
/// separate entry for every browser's exact ordering.
pub(crate) fn normalize_accept_encoding(req: &RequestMetadata<'_>) -> String {
    // Preserve coding order and quality values. A compact br/gzip/identity
    // bucket is unsafe: origins may choose a different representation based on
    // weights, wildcard codings, or newer codings such as zstd.
    // Rendered rather than decoded: a header value `to_str` cannot read used to
    // be dropped, which made the request identical to one that sent no
    // `Accept-Encoding` at all and let it share that entry.
    let joined = req
        .headers
        .get_all(http::header::ACCEPT_ENCODING)
        .iter()
        .map(crate::classifier::header_text)
        .collect::<Vec<_>>()
        .join(",");
    let values = joined
        .split(',')
        .map(|part| {
            part.split(';')
                .map(|piece| piece.trim().to_ascii_lowercase().replace(' ', ""))
                .collect::<Vec<_>>()
                .join(";")
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        "identity".into()
    } else {
        values.join(",")
    }
}

fn header_values(req: &RequestMetadata<'_>, name: &str) -> Option<String> {
    // Rendered rather than decoded. Dropping a value `to_str` could not read
    // made the request indistinguishable from one that never sent the header,
    // so two clients asking for different variants shared an entry.
    let values = req
        .headers
        .get_all(name)
        .iter()
        .map(crate::classifier::header_text)
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join("\u{1d}"))
}

/// Filter and sort the query string.
///
/// With no policy, every parameter is kept. That is correct — dropping a
/// parameter merges semantically different pages — but it also means a unique
/// parameter per request mints a unique key per request, and therefore a render
/// per request. An `include` allowlist is the fix, and admission control is
/// what bounds the damage until someone configures one.
pub(crate) fn canonical_query(raw: &str, policy: Option<&QueryPolicy>) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, Option<&str>)> = raw
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (p, None),
        })
        .filter(|(k, _)| match policy {
            None => true,
            Some(p) => {
                let listed = p.keys.iter().any(|want| want == k);
                match p.mode {
                    QueryMode::Include => listed,
                    QueryMode::Exclude => !listed,
                }
            }
        })
        .collect();
    // Reordering duplicate keys can change application semantics. Only sort
    // when every key is unique; otherwise retain the request's original order.
    let mut seen = std::collections::HashSet::new();
    if pairs.iter().all(|(key, _)| seen.insert(*key)) {
        pairs.sort_unstable();
    }
    pairs
        .iter()
        .map(|(k, v)| match v {
            Some(v) => format!("{k}={v}"),
            None => (*k).to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method};

    struct Ctx {
        headers: HeaderMap,
    }

    impl Ctx {
        fn new() -> Self {
            Ctx {
                headers: HeaderMap::new(),
            }
        }
        fn with(mut self, k: &'static str, v: &'static str) -> Self {
            self.headers.insert(k, HeaderValue::from_static(v));
            self
        }
    }

    fn key(
        path: &str,
        query: Option<&str>,
        ctx: &Ctx,
        variant: &[&str],
        deployment: Option<&str>,
    ) -> CacheKey {
        let req = RequestMetadata {
            method: &Method::GET,
            host: "shop.example.com",
            path,
            query,
            headers: &ctx.headers,
        };
        let variant = variant
            .iter()
            .map(|header| (*header).to_string())
            .collect::<Vec<_>>();
        KeyBuilder {
            scheme: "https",
            forwarded_host: None,
            query_policy: None,
            variant_headers: &variant,
            deployment,
        }
        .build(&req)
    }

    #[test]
    fn query_parameter_order_does_not_split_the_entry() {
        let c = Ctx::new();
        assert_eq!(
            key("/p", Some("a=1&b=2"), &c, &[], None),
            key("/p", Some("b=2&a=1"), &c, &[], None)
        );
    }

    #[test]
    fn different_paths_are_different_keys() {
        let c = Ctx::new();
        assert_ne!(
            key("/a", None, &c, &[], None),
            key("/b", None, &c, &[], None)
        );
    }

    #[test]
    fn rsc_variant_separates_flight_payload_from_document() {
        // The bug this prevents: a browser navigating to /products/iphone
        // receiving the RSC flight payload instead of HTML.
        let doc = Ctx::new();
        let rsc = Ctx::new().with("rsc", "1");
        assert_ne!(
            key("/products/iphone", None, &doc, &["rsc"], None),
            key("/products/iphone", None, &rsc, &["rsc"], None)
        );
    }

    /// Found alongside the `cookies` fuzz finding.
    ///
    /// `HeaderValue` accepts obs-text that `to_str` refuses, and the variant
    /// values were collected with `.to_str().ok()` — so a header carrying one
    /// such byte was dropped, making the request identical to one that never
    /// sent the header, and to every other request whose value was equally
    /// unreadable. Three clients asking for three different variants shared
    /// one entry.
    #[test]
    fn variant_values_that_are_not_ascii_still_separate_entries() {
        let mut absent = HeaderMap::new();
        let _ = &mut absent;
        let mut first = HeaderMap::new();
        first.insert("x-variant", HeaderValue::from_bytes(b"\xd0\xa5").unwrap());
        let mut second = HeaderMap::new();
        second.insert("x-variant", HeaderValue::from_bytes(b"\xd0\xa6").unwrap());

        let build = |headers: &HeaderMap| {
            KeyBuilder {
                scheme: "https",
                forwarded_host: None,
                query_policy: None,
                variant_headers: &["x-variant".to_string()],
                deployment: None,
            }
            .build(&RequestMetadata {
                method: &Method::GET,
                host: "shop.example.com",
                path: "/p",
                query: None,
                headers,
            })
        };

        assert_ne!(build(&first), build(&second));
        assert_ne!(build(&first), build(&absent));
    }

    #[test]
    fn brotli_and_gzip_clients_get_separate_entries() {
        let br = Ctx::new().with("accept-encoding", "gzip, deflate, br");
        let gz = Ctx::new().with("accept-encoding", "gzip, deflate");
        assert_ne!(
            key("/p", None, &br, &[], None),
            key("/p", None, &gz, &[], None)
        );
    }

    #[test]
    fn encoding_preference_order_remains_distinct() {
        let a = Ctx::new().with("accept-encoding", "br, gzip");
        let b = Ctx::new().with("accept-encoding", "gzip, br");
        assert_ne!(
            key("/p", None, &a, &[], None),
            key("/p", None, &b, &[], None)
        );
    }

    #[test]
    fn explicitly_refused_encoding_is_not_selected() {
        let zero = Ctx::new().with("accept-encoding", "br;q=0");
        let zero_decimal = Ctx::new().with("accept-encoding", "br;q=0.0");
        let accepts = Ctx::new().with("accept-encoding", "br");
        assert_ne!(
            key("/p", None, &zero, &[], None),
            key("/p", None, &accepts, &[], None)
        );
        assert_ne!(
            key("/p", None, &zero_decimal, &[], None),
            key("/p", None, &accepts, &[], None)
        );
    }

    /// Found by the `cache_key` fuzz target. `unwrap_or("")` rendered "no
    /// deployment id" and "a deployment id that is the empty string"
    /// identically, so two structurally distinct keys shared one entry.
    #[test]
    fn an_empty_deployment_id_is_not_the_same_as_none() {
        let c = Ctx::new();
        let none = key("/p", None, &c, &[], None);
        let empty = key("/p", None, &c, &[], Some(""));
        assert_ne!(none, empty);
        assert_ne!(none.canonical_string(), empty.canonical_string());
    }

    #[test]
    fn deployment_id_prevents_reuse_across_builds() {
        let c = Ctx::new();
        assert_ne!(
            key("/p", None, &c, &[], Some("build-a")),
            key("/p", None, &c, &[], Some("build-b"))
        );
    }

    #[test]
    fn host_case_and_default_port_normalize() {
        let c = Ctx::new();
        let mut a = key("/p", None, &c, &[], None);
        a.host = normalize_host("SHOP.example.com:443", "https");
        assert_eq!(a, key("/p", None, &c, &[], None));
    }

    #[test]
    fn non_default_ports_are_not_removed() {
        assert_ne!(
            normalize_host("example.com:443", "http"),
            normalize_host("example.com", "http")
        );
        assert_ne!(
            normalize_host("example.com:80", "https"),
            normalize_host("example.com", "https")
        );
    }

    #[test]
    fn include_policy_drops_unlisted_parameters() {
        let p = QueryPolicy {
            mode: QueryMode::Include,
            keys: vec!["q".into(), "page".into()],
        };
        // A cache-busting parameter must not mint a new key, or every request
        // becomes a render.
        assert_eq!(
            canonical_query("q=shoes&cachebust=91821", Some(&p)),
            "q=shoes"
        );
        assert_eq!(
            canonical_query("q=shoes&page=2", Some(&p)),
            "page=2&q=shoes"
        );
    }

    #[test]
    fn exclude_policy_drops_only_listed_parameters() {
        let p = QueryPolicy {
            mode: QueryMode::Exclude,
            keys: vec!["utm_source".into()],
        };
        assert_eq!(
            canonical_query("utm_source=fb&q=shoes", Some(&p)),
            "q=shoes"
        );
    }

    #[test]
    fn no_policy_keeps_every_parameter() {
        assert_eq!(canonical_query("a=1&zz=2", None), "a=1&zz=2");
    }

    #[test]
    fn duplicate_query_parameter_order_is_preserved() {
        assert_ne!(
            canonical_query("step=a&step=b", None),
            canonical_query("step=b&step=a", None)
        );
    }

    #[test]
    fn valueless_and_empty_query_parameters_are_distinct() {
        assert_ne!(
            canonical_query("flag", None),
            canonical_query("flag=", None)
        );
    }

    /// The exact encoding, pinned: changing it moves every stored entry to a
    /// new key, so it must never change as a side effect of how it is built.
    #[test]
    fn the_canonical_encoding_is_byte_for_byte_stable() {
        let ctx = Ctx::new().with("rsc", "1").with("accept-encoding", "gzip");
        let key = key("/p", Some("a=1"), &ctx, &["RSC"], Some("d1"));
        let expected = "5\u{1e}https\u{1f}16\u{1e}shop.example.com\u{1f}3\u{1e}GET\u{1f}\
                        2\u{1e}/p\u{1f}3\u{1e}a=1\u{1f}1\u{1e}d\u{1f}2\u{1e}d1\u{1f}\
                        1\u{1e}2\u{1f}3\u{1e}rsc\u{1f}1\u{1e}1\u{1f}\
                        9\u{1e}~encoding\u{1f}4\u{1e}gzip\u{1f}";
        assert_eq!(key.canonical_string(), expected);
        let pingora = CacheNamespace(7).key_for(&key, "/p");
        assert_eq!(
            pingora.primary_key_str(),
            Some(format!("tanod-policy-v1:7:{expected}").as_str())
        );
        assert_eq!(
            pingora_cache::key::CacheHashKey::combined(&pingora),
            pingora_cache::key::CacheHashKey::combined(
                &CacheNamespace(7).key(key.canonical_string(), "/p")
            )
        );
    }

    #[test]
    fn canonical_string_cannot_confuse_component_boundaries() {
        let c = Ctx::new();
        // Without a separator that cannot appear in a component, these two
        // would render to the same string.
        let a = key("/b", None, &c, &[], None);
        let mut b = key("", None, &c, &[], None);
        b.host = "shop.example.com/b".into();
        assert_ne!(a.canonical_string(), b.canonical_string());
    }

    #[test]
    fn canonical_string_matches_key_equality() {
        let c = Ctx::new();
        let a = key("/p", Some("a=1&b=2"), &c, &[], None);
        let b = key("/p", Some("b=2&a=1"), &c, &[], None);
        assert_eq!(a, b);
        assert_eq!(a.canonical_string(), b.canonical_string());
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishing() {
        let c = Ctx::new();
        let a = key("/a", None, &c, &[], None);
        let b = key("/b", None, &c, &[], None);
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}

/// Property tests for the parts of keying where a counterexample is a security
/// bug rather than a wrong number.
///
/// The hand-written tests above pin the behaviours somebody thought to check.
/// These assert the invariants that must hold for *every* input, including the
/// malformed HTTP metadata a caller can put on the wire — a header value with
/// the encoding's own separator bytes in it, a query string that is nothing but
/// `&`, a path of control characters.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::config::schema::{QueryMode, QueryPolicy};
    use http::{HeaderMap, HeaderName, HeaderValue, Method};
    use proptest::prelude::*;

    /// Component text drawn deliberately wider than HTTP permits: control
    /// characters, the encoding's own separators, `&` and `=`. Injectivity of
    /// the canonical encoding must not depend on `http`'s validation.
    fn component() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                60 => prop::char::range('!', '~'),
                10 => Just('\u{1f}'),
                10 => Just('\u{1e}'),
                10 => Just('&'),
                10 => Just('='),
            ],
            0..12,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    /// Text guaranteed to contain none of the encoding's own bytes, so a
    /// constructed collision is unambiguous about where the boundary moved.
    fn safe() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-z0-9]{0,6}").unwrap()
    }

    #[allow(clippy::too_many_arguments)] // it mirrors the shape of a request
    fn key_from(
        scheme: &str,
        host: &str,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
        variant: &[String],
        deployment: Option<&str>,
    ) -> CacheKey {
        KeyBuilder {
            scheme,
            forwarded_host: None,
            query_policy: None,
            variant_headers: variant,
            deployment,
        }
        .build(&RequestMetadata {
            method,
            host,
            path,
            query,
            headers,
        })
    }

    /// Assemble a key from raw components, bypassing `KeyBuilder`.
    ///
    /// The encoding has to be injective on its own terms. Driving it only
    /// through `KeyBuilder` cannot demonstrate that: `http` rejects control
    /// characters in header values, so a separator byte never reaches the
    /// encoder by that route, and the property passes whether or not the
    /// encoding is actually injective. Constructing the struct directly tests
    /// the function rather than the validation upstream of it.
    fn raw_key(parts: &[String], variant: &[(String, String)]) -> CacheKey {
        CacheKey {
            scheme: parts[0].clone(),
            host: parts[1].clone(),
            method: parts[2].clone(),
            path: parts[3].clone(),
            query: parts[4].clone(),
            variant: variant.to_vec(),
            deployment: Some(parts[5].clone()),
        }
    }

    fn parts() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(component(), 6..=6)
    }

    fn variants() -> impl Strategy<Value = Vec<(String, String)>> {
        prop::collection::vec((component(), component()), 0..4)
    }

    proptest! {
        /// The invariant everything else rests on. `pingora-cache` hashes the
        /// canonical string, so two keys that are structurally different but
        /// render identically are one visitor's page stored under another
        /// visitor's key.
        ///
        /// The generator emits the encoding's own separator bytes, `&` and `=`
        /// far more often than chance would, because the collisions worth
        /// finding are the ones where a component smuggles a boundary.
        #[test]
        fn canonical_string_is_injective(
            left_parts in parts(), left_variant in variants(),
            right_parts in parts(), right_variant in variants(),
        ) {
            let a = raw_key(&left_parts, &left_variant);
            let b = raw_key(&right_parts, &right_variant);
            prop_assert_eq!(a == b, a.canonical_string() == b.canonical_string());
        }

        /// Boundary shifting, constructed rather than searched for.
        ///
        /// Random generation will not find this: it needs one key's components
        /// to be the *same text as another's* with the boundary moved, and
        /// two independent draws never agree that way. Building the pair by
        /// hand is what gives the property teeth — with a separator-only
        /// encoding, `host = "x\u{1f}y", method = "z"` and `host = "x",
        /// method = "y\u{1f}z"` render to the same string and share a cache
        /// entry.
        #[test]
        fn a_separator_in_a_component_cannot_shift_a_boundary(
            base in parts(), x in safe(), y in safe(), z in safe(),
        ) {
            let sep = '\u{1f}';
            let mut left = base.clone();
            left[1] = format!("{x}{sep}{y}");
            left[2] = z.clone();

            let mut right = base;
            right[1] = x;
            right[2] = format!("{y}{sep}{z}");

            let a = raw_key(&left, &[]);
            let b = raw_key(&right, &[]);
            prop_assert_ne!(&a, &b);
            prop_assert_ne!(a.canonical_string(), b.canonical_string());
        }

        /// The same construction against the variable-length half, where it is
        /// cheaper still: one variant whose value carries a separator renders
        /// exactly like two variants under a separator-only encoding.
        #[test]
        fn a_separator_in_a_variant_value_cannot_forge_an_extra_variant(
            base in parts(),
            first_name in safe(), first_value in safe(),
            second_name in safe(), second_value in safe(),
        ) {
            let two = raw_key(&base, &[
                (first_name.clone(), first_value.clone()),
                (second_name.clone(), second_value.clone()),
            ]);
            let smuggled = raw_key(&base, &[(
                first_name,
                format!("{first_value}\u{1f}{second_name}\u{1e}{second_value}"),
            )]);
            prop_assert_ne!(&two, &smuggled);
            prop_assert_ne!(two.canonical_string(), smuggled.canonical_string());
        }

        /// And through the real builder, where the values come from headers
        /// `http` has already validated. This is the reachable-in-production
        /// half of the same claim.
        #[test]
        fn variant_values_cannot_forge_another_key(
            left in prop::collection::vec(component(), 0..3),
            right in prop::collection::vec(component(), 0..3),
        ) {
            let names = ["x-a", "x-b", "x-c"];
            let build = |values: &[String]| {
                let mut headers = HeaderMap::new();
                let mut wanted = Vec::new();
                for (name, value) in names.iter().zip(values) {
                    // Skip values `http` itself would reject; an
                    // unrepresentable header never arrives.
                    if let Ok(v) = HeaderValue::from_str(value) {
                        headers.insert(HeaderName::from_static(name), v);
                        wanted.push((*name).to_string());
                    }
                }
                key_from("https", "h", &Method::GET, "/p", None, &headers, &wanted, None)
            };
            let a = build(&left);
            let b = build(&right);
            prop_assert_eq!(a == b, a.canonical_string() == b.canonical_string());
        }

        /// A deployment id must partition the key space. Two builds sharing an
        /// entry is how a deploy serves the previous build's HTML against the
        /// new build's asset hashes.
        #[test]
        fn deployment_ids_never_share_an_entry(
            first in component(),
            second in component(),
            path in component(),
        ) {
            prop_assume!(first != second);
            let headers = HeaderMap::new();
            let a = key_from("https", "h", &Method::GET, &path, None, &headers, &[], Some(&first));
            let b = key_from("https", "h", &Method::GET, &path, None, &headers, &[], Some(&second));
            prop_assert_ne!(a.canonical_string(), b.canonical_string());
        }

        /// Reordering a query string must not mint a second entry — that is a
        /// render per permutation — but only where reordering is meaning
        /// preserving, which duplicate keys are not.
        #[test]
        fn unique_query_parameters_are_order_insensitive(
            pairs in prop::collection::vec(
                (prop::string::string_regex("[a-z]{1,6}").unwrap(),
                 prop::string::string_regex("[a-z0-9]{0,6}").unwrap()),
                0..6,
            ),
        ) {
            let mut seen = std::collections::HashSet::new();
            prop_assume!(pairs.iter().all(|(k, _)| seen.insert(k.clone())));

            let forward = pairs.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let reversed = pairs.iter().rev()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            prop_assert_eq!(canonical_query(&forward, None), canonical_query(&reversed, None));
        }

        /// Canonicalising an already-canonical query must be a no-op, or the
        /// key depends on how many times the value happened to pass through.
        #[test]
        fn canonical_query_is_idempotent(raw in component()) {
            let once = canonical_query(&raw, None);
            prop_assert_eq!(canonical_query(&once, None), once);
        }

        /// An `include` allowlist is what stops a cache-busting parameter from
        /// minting a render per request. Nothing outside the list may survive.
        #[test]
        fn include_policy_admits_only_listed_keys(
            raw in prop::string::string_regex("([a-z]{1,4}=[a-z0-9]{0,4}&){0,5}[a-z]{1,4}=[a-z0-9]{0,4}").unwrap(),
            allowed in prop::collection::vec(prop::string::string_regex("[a-z]{1,4}").unwrap(), 0..4),
        ) {
            let policy = QueryPolicy { mode: QueryMode::Include, keys: allowed.clone() };
            for pair in canonical_query(&raw, Some(&policy)).split('&').filter(|p| !p.is_empty()) {
                let key = pair.split('=').next().unwrap_or("");
                prop_assert!(allowed.iter().any(|a| a == key), "kept unlisted key `{}`", key);
            }
        }

        /// And the mirror image: an `exclude` list must remove every key it
        /// names, whatever else is in the string.
        #[test]
        fn exclude_policy_removes_every_listed_key(
            raw in prop::string::string_regex("([a-z]{1,4}=[a-z0-9]{0,4}&){0,5}[a-z]{1,4}=[a-z0-9]{0,4}").unwrap(),
            denied in prop::collection::vec(prop::string::string_regex("[a-z]{1,4}").unwrap(), 0..4),
        ) {
            let policy = QueryPolicy { mode: QueryMode::Exclude, keys: denied.clone() };
            for pair in canonical_query(&raw, Some(&policy)).split('&').filter(|p| !p.is_empty()) {
                let key = pair.split('=').next().unwrap_or("");
                prop_assert!(!denied.iter().any(|d| d == key), "kept excluded key `{}`", key);
            }
        }

        /// Two clients that named different content codings must never share a
        /// stored body, however the header was spelled.
        #[test]
        fn distinct_accept_encodings_do_not_share_an_entry(
            left in prop::string::string_regex("(gzip|br|zstd|identity|\\*)(;q=0\\.[0-9])?(, ?(gzip|br|zstd))?").unwrap(),
            right in prop::string::string_regex("(gzip|br|zstd|identity|\\*)(;q=0\\.[0-9])?(, ?(gzip|br|zstd))?").unwrap(),
        ) {
            let build = |raw: &str| {
                let mut headers = HeaderMap::new();
                headers.insert(http::header::ACCEPT_ENCODING, HeaderValue::from_str(raw).unwrap());
                key_from("https", "h", &Method::GET, "/p", None, &headers, &[], None)
            };
            let a = build(&left);
            let b = build(&right);
            // Normalisation only strips whitespace and case, so anything that
            // survives as a different string must land in a different entry.
            let normalised_apart = {
                let mut ha = HeaderMap::new();
                ha.insert(http::header::ACCEPT_ENCODING, HeaderValue::from_str(&left).unwrap());
                let mut hb = HeaderMap::new();
                hb.insert(http::header::ACCEPT_ENCODING, HeaderValue::from_str(&right).unwrap());
                let meta = |h: &HeaderMap| normalize_accept_encoding(&RequestMetadata {
                    method: &Method::GET, host: "h", path: "/p", query: None, headers: h,
                });
                meta(&ha) != meta(&hb)
            };
            if normalised_apart {
                prop_assert_ne!(a.canonical_string(), b.canonical_string());
            }
        }

        /// Nothing in keying may panic. A panic in the request path is a denial
        /// of service reachable by anyone who can send a header.
        #[test]
        fn malformed_metadata_never_panics(
            scheme in component(), host in component(), path in component(),
            query in component(), header_value in component(),
            method in prop::sample::select(vec!["GET", "HEAD", "POST", "PURGE", "GTE"]),
        ) {
            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&header_value) {
                headers.insert(HeaderName::from_static("x-probe"), v);
            }
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let key = key_from(
                &scheme, &host, &method, &path, Some(&query),
                &headers, &["x-probe".to_string()], Some(&host),
            );
            // Deriving both views must also be total.
            let _ = key.canonical_string();
            let _ = key.fingerprint();
        }
    }
}
