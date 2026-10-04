//! May this response be shared, and for how long?
//!
//! Evaluated twice, because the two questions are answered at different times:
//!
//! * [`evaluate_request`] runs before the origin is touched, and decides
//!   whether reuse is even worth attempting.
//! * [`evaluate_response`] runs when the origin answers, and is the one that
//!   matters for safety. Shareability is a property of the *response*: a route
//!   that looks public can still return a `Set-Cookie`, and by then there may
//!   be hundreds of waiters attached to it.

use crate::classifier::{RequestClass, RequestMetadata};
use crate::config::schema::RouteCache;
use std::time::Duration;

/// Why a response will not be shared. Recorded on the request for logs and
/// the `tanod_cache_bypass_total` counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BypassReason {
    UnsafeMethod,
    Authorization,
    Cookie,
    ClassNotStorable,
    RouteDisabled,
    Framework(&'static str),
    Status(u16),
    /// The response sets a cookie. Absolute: no override reaches this.
    SetCookie,
    CacheControlPrivate,
    NoStore,
    NoCache,
    /// The origin declared it varies on something the key does not carry.
    UnsupportedVary(String),
}

impl BypassReason {
    pub fn as_str(&self) -> &str {
        match self {
            BypassReason::UnsafeMethod => "unsafe_method",
            BypassReason::Authorization => "authorization",
            BypassReason::Cookie => "cookie",
            BypassReason::ClassNotStorable => "class_not_storable",
            BypassReason::RouteDisabled => "route_disabled",
            BypassReason::Framework(f) => f,
            BypassReason::Status(_) => "status",
            BypassReason::SetCookie => "set_cookie",
            BypassReason::CacheControlPrivate => "cache_control_private",
            BypassReason::NoStore => "no_store",
            BypassReason::NoCache => "no_cache",
            BypassReason::UnsupportedVary(_) => "unsupported_vary",
        }
    }
}

/// What reuse may be attempted for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Bypass(BypassReason),
    Eligible { may_store: bool, may_coalesce: bool },
}

/// What may be done with the response that came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shareability {
    Shareable {
        ttl: Duration,
        swr: Duration,
        sie: Duration,
    },
    /// Safe to hand to the waiters already attached to this flight, but not to
    /// store. Used for `coalesce.override_origin` and for high-cardinality
    /// variants like Next prefetch payloads.
    TransientOnly,
    NotShareable(BypassReason),
}

/// Parsed `Cache-Control`. Unknown directives are ignored, absent ones are
/// `None` — "the origin said nothing" and "the origin said zero" are different.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheControl {
    pub no_store: bool,
    pub no_cache: bool,
    pub private: bool,
    pub public: bool,
    pub max_age: Option<u64>,
    pub s_maxage: Option<u64>,
    pub stale_while_revalidate: Option<u64>,
    pub stale_if_error: Option<u64>,
}

impl CacheControl {
    pub fn parse(raw: &str) -> CacheControl {
        let mut cc = CacheControl::default();
        for part in raw.split(',') {
            let part = part.trim();
            let (name, value) = match part.split_once('=') {
                Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"'))),
                None => (part, None),
            };
            let secs = || value.and_then(|v| v.parse::<u64>().ok());
            match name.to_ascii_lowercase().as_str() {
                "no-store" => cc.no_store = true,
                "no-cache" => cc.no_cache = true,
                "private" => cc.private = true,
                "public" => cc.public = true,
                "max-age" => cc.max_age = secs(),
                "s-maxage" => cc.s_maxage = secs(),
                "stale-while-revalidate" => cc.stale_while_revalidate = secs(),
                "stale-if-error" => cc.stale_if_error = secs(),
                _ => {}
            }
        }
        cc
    }

    /// Shared caches honour `s-maxage` ahead of `max-age`.
    pub fn shared_ttl(&self) -> Option<Duration> {
        self.s_maxage.or(self.max_age).map(Duration::from_secs)
    }
}

/// Metadata from the origin's response, reduced to what the decision reads.
pub struct ResponseMetadata<'a> {
    pub status: u16,
    pub cache_control: Option<&'a str>,
    pub set_cookie: bool,
    pub vary: Option<&'a str>,
}

const CACHEABLE_STATUSES: &[u16] = &[200, 203, 204, 301, 308, 404, 410];

pub fn evaluate_request(
    req: &RequestMetadata<'_>,
    class: RequestClass,
    framework_bypass: Option<&'static str>,
    route: Option<&RouteCache>,
    coalesce_override: bool,
    cache_enabled: bool,
    coalesce_enabled: bool,
) -> Disposition {
    if let Some(f) = framework_bypass {
        return Disposition::Bypass(BypassReason::Framework(f));
    }
    if !req.is_safe_method() {
        return Disposition::Bypass(BypassReason::UnsafeMethod);
    }
    if req.has_authorization() {
        return Disposition::Bypass(BypassReason::Authorization);
    }
    if !cache_enabled && !coalesce_enabled {
        return Disposition::Bypass(BypassReason::RouteDisabled);
    }
    if !class.storable_in_principle() && !class.coalescible_in_principle() {
        return Disposition::Bypass(BypassReason::ClassNotStorable);
    }
    // A cookie-bearing request is personalised unless the route has explicitly
    // claimed otherwise by overriding.
    let override_origin = route.is_some_and(|r| r.override_origin);
    if req.has_cookies() && !override_origin && !coalesce_override {
        return Disposition::Bypass(BypassReason::Cookie);
    }
    Disposition::Eligible {
        may_store: cache_enabled && class.storable_in_principle(),
        may_coalesce: coalesce_enabled && class.coalescible_in_principle(),
    }
}

/// The decision that actually protects users.
///
/// `key_headers` is every header the cache key carries. If the origin says it
/// varies on something outside that set, the stored entry would be served to
/// requests that should have got a different body — so it is not stored.
pub fn evaluate_response(
    res: &ResponseMetadata<'_>,
    route: Option<&RouteCache>,
    key_headers: &[String],
    coalesce_only: bool,
    coalesce_override: bool,
) -> Shareability {
    // --- Absolute rules. No configuration reaches past these. ---

    // A response that sets a cookie is addressed to one client. Storing it, or
    // handing it to the waiters attached to this flight, distributes somebody's
    // session to strangers.
    if res.set_cookie {
        return Shareability::NotShareable(BypassReason::SetCookie);
    }

    if let Some(vary) = res.vary
        && let Some(offending) = unsupported_vary(vary, key_headers)
    {
        return Shareability::NotShareable(BypassReason::UnsupportedVary(offending));
    }

    if !CACHEABLE_STATUSES.contains(&res.status) {
        return Shareability::NotShareable(BypassReason::Status(res.status));
    }

    // --- Origin directives, which a fenced route override may outrank. ---

    let cc = res
        .cache_control
        .map(CacheControl::parse)
        .unwrap_or_default();
    let override_origin = route.is_some_and(|r| r.override_origin);
    let route_max = route
        .and_then(|r| r.ttl.as_ref())
        .and_then(|t| t.max)
        .map(|d| d.as_duration());

    if !override_origin && !coalesce_override {
        if cc.no_store {
            return Shareability::NotShareable(BypassReason::NoStore);
        }
        if cc.private {
            return Shareability::NotShareable(BypassReason::CacheControlPrivate);
        }
        if cc.no_cache {
            return Shareability::NotShareable(BypassReason::NoCache);
        }
    }

    if coalesce_only {
        return Shareability::TransientOnly;
    }

    // An override supplies the TTL the origin refused to. Validation has
    // already guaranteed `ttl.max` is present whenever `override_origin` is.
    let ttl = if override_origin {
        // The override disregards the origin's directives wholesale. Taking
        // `min(origin, route)` would let the `max-age=0` that Next sends on a
        // dynamic route pin the result to zero and quietly disable the very
        // microcache the override was written to enable.
        match route_max {
            Some(m) => m,
            None => return Shareability::NotShareable(BypassReason::NoCache),
        }
    } else {
        match (cc.shared_ttl(), route_max) {
            (Some(o), Some(m)) => o.min(m), // route policy shrinks, never grows
            (Some(o), None) => o,
            (None, _) => return Shareability::NotShareable(BypassReason::NoCache),
        }
    };

    if ttl.is_zero() {
        return Shareability::TransientOnly;
    }

    Shareability::Shareable {
        ttl,
        swr: route
            .and_then(|r| r.stale_while_revalidate)
            .map(|d| d.as_duration())
            .or_else(|| cc.stale_while_revalidate.map(Duration::from_secs))
            .unwrap_or_default(),
        sie: route
            .and_then(|r| r.stale_if_error)
            .map(|d| d.as_duration())
            .or_else(|| cc.stale_if_error.map(Duration::from_secs))
            .unwrap_or_default(),
    }
}

/// Returns the first `Vary` token the cache key cannot honour.
pub(crate) fn unsupported_vary(vary: &str, key_headers: &[String]) -> Option<String> {
    for token in vary.split(',') {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        if token == "*" {
            return Some("*".into());
        }
        // Already part of the key by construction.
        if token == "accept-encoding" {
            continue;
        }
        if !key_headers.iter().any(|k| k.eq_ignore_ascii_case(&token)) {
            return Some(token);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{RouteCache, Ttl};
    use crate::config::units::Dur;
    use http::{HeaderMap, Method};

    fn res<'a>(status: u16, cc: Option<&'a str>) -> ResponseMetadata<'a> {
        ResponseMetadata {
            status,
            cache_control: cc,
            set_cookie: false,
            vary: None,
        }
    }

    fn route_with_override(ttl_secs: u64) -> RouteCache {
        RouteCache {
            override_origin: true,
            ttl: Some(Ttl {
                max: Some(Dur(Duration::from_secs(ttl_secs))),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn parses_cache_control_directives() {
        let cc = CacheControl::parse("public, s-maxage=60, stale-while-revalidate=30");
        assert!(cc.public);
        assert_eq!(cc.s_maxage, Some(60));
        assert_eq!(cc.stale_while_revalidate, Some(30));
    }

    #[test]
    fn s_maxage_outranks_max_age_for_a_shared_cache() {
        let cc = CacheControl::parse("max-age=600, s-maxage=60");
        assert_eq!(cc.shared_ttl(), Some(Duration::from_secs(60)));
    }

    #[test]
    fn private_response_is_never_stored() {
        let r = res(200, Some("private, no-store"));
        assert_eq!(
            evaluate_response(&r, None, &[], false, false),
            Shareability::NotShareable(BypassReason::NoStore)
        );
    }

    #[test]
    fn set_cookie_beats_every_override() {
        // The rule the spec was missing. A fenced public route that returns a
        // session cookie must still not be shared.
        let r = ResponseMetadata {
            status: 200,
            cache_control: Some("public, s-maxage=60"),
            set_cookie: true,
            vary: None,
        };
        let route = route_with_override(2);
        assert_eq!(
            evaluate_response(&r, Some(&route), &[], false, false),
            Shareability::NotShareable(BypassReason::SetCookie)
        );
    }

    #[test]
    fn origin_vary_outside_the_key_blocks_storage() {
        let r = ResponseMetadata {
            status: 200,
            cache_control: Some("public, s-maxage=60"),
            set_cookie: false,
            vary: Some("Cookie"),
        };
        assert!(matches!(
            evaluate_response(&r, None, &["rsc".into()], false, false),
            Shareability::NotShareable(BypassReason::UnsupportedVary(_))
        ));
    }

    #[test]
    fn origin_vary_inside_the_key_is_fine() {
        let r = ResponseMetadata {
            status: 200,
            cache_control: Some("public, s-maxage=60"),
            set_cookie: false,
            vary: Some("RSC, Accept-Encoding"),
        };
        assert!(matches!(
            evaluate_response(&r, None, &["rsc".into()], false, false),
            Shareability::Shareable { .. }
        ));
    }

    #[test]
    fn vary_star_is_never_shareable() {
        let r = ResponseMetadata {
            status: 200,
            cache_control: Some("public, s-maxage=60"),
            set_cookie: false,
            vary: Some("*"),
        };
        assert!(matches!(
            evaluate_response(&r, None, &["rsc".into()], false, false),
            Shareability::NotShareable(BypassReason::UnsupportedVary(_))
        ));
    }

    #[test]
    fn route_ttl_ceiling_shrinks_but_never_grows_origin_ttl() {
        let route = RouteCache {
            ttl: Some(Ttl {
                max: Some(Dur(Duration::from_secs(2))),
            }),
            ..Default::default()
        };
        // origin 60s, route ceiling 2s -> 2s
        let r = res(200, Some("public, s-maxage=60"));
        assert_eq!(
            evaluate_response(&r, Some(&route), &[], false, false),
            Shareability::Shareable {
                ttl: Duration::from_secs(2),
                swr: Duration::ZERO,
                sie: Duration::ZERO
            }
        );
        // origin 1s, route ceiling 2s -> 1s, not 2s
        let r = res(200, Some("public, s-maxage=1"));
        assert!(matches!(
            evaluate_response(&r, Some(&route), &[], false, false),
            Shareability::Shareable { ttl, .. } if ttl == Duration::from_secs(1)
        ));
    }

    #[test]
    fn override_lets_a_no_store_next_route_microcache() {
        // Without this, a dynamically rendered Next page is never cached and
        // the product's headline demo cannot run.
        let r = res(
            200,
            Some("private, no-cache, no-store, max-age=0, must-revalidate"),
        );
        let route = route_with_override(2);
        assert!(matches!(
            evaluate_response(&r, Some(&route), &[], false, false),
            Shareability::Shareable { ttl, .. } if ttl == Duration::from_secs(2)
        ));
    }

    #[test]
    fn coalesce_only_shares_the_flight_without_storing() {
        let r = res(200, Some("public, s-maxage=60"));
        assert_eq!(
            evaluate_response(&r, None, &[], true, false),
            Shareability::TransientOnly
        );
    }

    #[test]
    fn uncacheable_status_is_not_shared() {
        let r = res(500, Some("public, s-maxage=60"));
        assert_eq!(
            evaluate_response(&r, None, &[], false, false),
            Shareability::NotShareable(BypassReason::Status(500))
        );
    }

    #[test]
    fn silent_origin_means_no_storage() {
        // No Cache-Control at all is not permission to cache.
        let r = res(200, None);
        assert!(matches!(
            evaluate_response(&r, None, &[], false, false),
            Shareability::NotShareable(_)
        ));
    }

    #[test]
    fn coalesce_override_can_share_no_store_for_one_flight_only() {
        let r = res(200, Some("private, no-cache, no-store"));
        assert_eq!(
            evaluate_response(&r, None, &[], true, true),
            Shareability::TransientOnly
        );
    }

    #[test]
    fn coalescing_can_remain_enabled_when_persistent_caching_is_off() {
        let headers = HeaderMap::new();
        let req = RequestMetadata {
            method: &Method::GET,
            host: "example.com",
            path: "/page",
            query: None,
            headers: &headers,
        };
        assert_eq!(
            evaluate_request(
                &req,
                RequestClass::PublicDocument,
                None,
                None,
                false,
                false,
                true,
            ),
            Disposition::Eligible {
                may_store: false,
                may_coalesce: true
            }
        );
    }

    #[test]
    fn disabling_cache_and_coalescing_bypasses_pingora_cache_entirely() {
        let headers = HeaderMap::new();
        let req = RequestMetadata {
            method: &Method::GET,
            host: "example.com",
            path: "/page",
            query: None,
            headers: &headers,
        };
        assert_eq!(
            evaluate_request(
                &req,
                RequestClass::PublicDocument,
                None,
                None,
                false,
                false,
                false,
            ),
            Disposition::Bypass(BypassReason::RouteDisabled)
        );
    }
}

/// Property tests for the rules whose failure mode is a wrong response rather
/// than a slow one.
///
/// The absolute barriers — `Set-Cookie`, an unsupported `Vary` — are stated in
/// the documentation as holding for *every* configuration. That is a universal
/// claim, and a universal claim is what a property test is for: an example
/// test only ever shows it held for the configurations somebody thought of.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::config::schema::{RouteCache, Ttl};
    use crate::config::units::Dur;
    use proptest::prelude::*;

    /// Header text drawn wide enough to include the separators and quoting a
    /// `Cache-Control` parser has to survive: commas, equals, semicolons,
    /// quotes and whitespace.
    fn header_text() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                40 => prop::char::range('a', 'z'),
                10 => prop::char::range('0', '9'),
                10 => Just(','),
                10 => Just('='),
                6 => Just('"'),
                6 => Just(' '),
                6 => Just('-'),
                6 => Just(';'),
                6 => Just('*'),
            ],
            0..40,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn directive_list() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::sample::select(vec![
                "no-store",
                "no-cache",
                "private",
                "public",
                "max-age=60",
                "s-maxage=30",
                "stale-while-revalidate=10",
                "stale-if-error=5",
                "must-revalidate",
                "immutable",
            ]),
            0..5,
        )
        .prop_map(|parts| parts.join(", "))
    }

    fn route() -> impl Strategy<Value = Option<RouteCache>> {
        prop::option::of((any::<bool>(), prop::option::of(1u64..600)).prop_map(
            |(override_origin, ttl_secs)| RouteCache {
                enabled: None,
                ttl: ttl_secs.map(|secs| Ttl {
                    max: Some(Dur(Duration::from_secs(secs))),
                }),
                stale_while_revalidate: None,
                stale_if_error: None,
                query: None,
                vary: None,
                override_origin,
            },
        ))
    }

    proptest! {
        /// The rule with no exceptions. A response that sets a cookie is
        /// addressed to one client; storing it, or handing it to the waiters
        /// already attached to this flight, distributes somebody's session to
        /// strangers. No route configuration and no override reaches past it.
        #[test]
        fn set_cookie_is_never_shareable(
            status in 100u16..600,
            cc in prop::option::of(directive_list()),
            vary in prop::option::of(header_text()),
            route in route(),
            key_headers in prop::collection::vec(prop::string::string_regex("x-[a-z]{1,5}").unwrap(), 0..4),
            coalesce_only in any::<bool>(),
            coalesce_override in any::<bool>(),
        ) {
            let res = ResponseMetadata {
                status,
                cache_control: cc.as_deref(),
                set_cookie: true,
                vary: vary.as_deref(),
            };
            let verdict = evaluate_response(
                &res, route.as_ref(), &key_headers, coalesce_only, coalesce_override,
            );
            prop_assert_eq!(verdict, Shareability::NotShareable(BypassReason::SetCookie));
        }

        /// The second absolute rule. If the origin says it varies on something
        /// the key does not carry, a stored entry would be served to requests
        /// that should have received a different body.
        #[test]
        fn vary_outside_the_key_is_never_shareable(
            status in prop::sample::select(CACHEABLE_STATUSES.to_vec()),
            cc in prop::option::of(directive_list()),
            route in route(),
            coalesce_only in any::<bool>(),
            coalesce_override in any::<bool>(),
            unknown in prop::string::string_regex("x-unkeyed-[a-z]{1,5}").unwrap(),
        ) {
            let res = ResponseMetadata {
                status,
                cache_control: cc.as_deref(),
                set_cookie: false,
                vary: Some(&unknown),
            };
            let verdict = evaluate_response(
                &res, route.as_ref(), &["accept-language".to_string()], coalesce_only, coalesce_override,
            );
            prop_assert_eq!(
                verdict,
                Shareability::NotShareable(BypassReason::UnsupportedVary(unknown.to_ascii_lowercase()))
            );
        }

        /// `Vary: *` means "no two requests are equivalent". There is no key
        /// that can honour it, so no configuration may claim otherwise.
        #[test]
        fn vary_star_is_never_shareable(
            padding in prop::collection::vec(prop::sample::select(vec!["accept-encoding", "x-a", ""]), 0..3),
            route in route(),
            key_headers in prop::collection::vec(prop::string::string_regex("x-[a-z]{1,5}").unwrap(), 0..4),
        ) {
            let mut tokens = padding;
            tokens.push("*");
            let vary = tokens.join(", ");
            let res = ResponseMetadata {
                status: 200,
                cache_control: Some("public, max-age=60"),
                set_cookie: false,
                vary: Some(&vary),
            };
            let verdict = evaluate_response(&res, route.as_ref(), &key_headers, false, false);
            prop_assert!(matches!(
                verdict,
                Shareability::NotShareable(BypassReason::UnsupportedVary(_))
            ));
        }

        /// A route's `ttl.max` is a ceiling, never a floor. Without an
        /// override it may only shrink what the origin asked for; with one it
        /// is the whole answer. Either way the stored entry never outlives it.
        #[test]
        fn route_ttl_max_is_never_exceeded(
            origin_secs in 0u64..3600,
            route_secs in 1u64..600,
            override_origin in any::<bool>(),
        ) {
            let cc = format!("public, max-age={origin_secs}");
            let route = RouteCache {
                enabled: None,
                ttl: Some(Ttl { max: Some(Dur(Duration::from_secs(route_secs))) }),
                stale_while_revalidate: None,
                stale_if_error: None,
                query: None,
                vary: None,
                override_origin,
            };
            let res = ResponseMetadata {
                status: 200,
                cache_control: Some(&cc),
                set_cookie: false,
                vary: None,
            };
            if let Shareability::Shareable { ttl, .. } =
                evaluate_response(&res, Some(&route), &[], false, false)
            {
                prop_assert!(ttl <= Duration::from_secs(route_secs));
            }
        }

        /// Without an override, the origin's ceiling is also binding. A route
        /// that could raise it would let a policy file overrule an origin that
        /// said the response goes stale in ten seconds.
        #[test]
        fn origin_ttl_binds_unless_overridden(
            origin_secs in 1u64..3600,
            route_secs in prop::option::of(1u64..3600),
        ) {
            let cc = format!("public, max-age={origin_secs}");
            let route = route_secs.map(|secs| RouteCache {
                enabled: None,
                ttl: Some(Ttl { max: Some(Dur(Duration::from_secs(secs))) }),
                stale_while_revalidate: None,
                stale_if_error: None,
                query: None,
                vary: None,
                override_origin: false,
            });
            let res = ResponseMetadata {
                status: 200,
                cache_control: Some(&cc),
                set_cookie: false,
                vary: None,
            };
            if let Shareability::Shareable { ttl, .. } =
                evaluate_response(&res, route.as_ref(), &[], false, false)
            {
                prop_assert!(ttl <= Duration::from_secs(origin_secs));
            }
        }

        /// A status outside the cacheable set is never stored, whatever the
        /// route says.
        #[test]
        fn uncacheable_statuses_are_never_stored(
            status in 100u16..600,
            route in route(),
            cc in prop::option::of(directive_list()),
        ) {
            prop_assume!(!CACHEABLE_STATUSES.contains(&status));
            let res = ResponseMetadata {
                status,
                cache_control: cc.as_deref(),
                set_cookie: false,
                vary: None,
            };
            let verdict = evaluate_response(&res, route.as_ref(), &[], false, false);
            prop_assert_eq!(verdict, Shareability::NotShareable(BypassReason::Status(status)));
        }

        /// `no-store` is the origin's clearest possible refusal. Only the
        /// fenced per-route override may outrank it, and validation refuses to
        /// pair that override with a private class or a cookie-bearing route.
        #[test]
        fn no_store_is_honoured_unless_explicitly_overridden(
            extra in directive_list(),
            status in prop::sample::select(CACHEABLE_STATUSES.to_vec()),
        ) {
            let cc = format!("no-store, {extra}");
            let res = ResponseMetadata {
                status,
                cache_control: Some(&cc),
                set_cookie: false,
                vary: None,
            };
            prop_assert_eq!(
                evaluate_response(&res, None, &[], false, false),
                Shareability::NotShareable(BypassReason::NoStore)
            );
        }

        /// Parsing must be total. `Cache-Control` is attacker-influenced on the
        /// way back from an origin that proxies third-party content, and a
        /// panic in the response path takes the proxy down.
        #[test]
        fn cache_control_parsing_never_panics(raw in header_text()) {
            let cc = CacheControl::parse(&raw);
            let _ = cc.shared_ttl();
        }

        /// Directives Tanod does not implement must be inert, not
        /// accidentally meaningful. Appending one may never change a decision.
        #[test]
        fn unknown_directives_do_not_change_the_parse(
            known in directive_list(),
            unknown in prop::string::string_regex("[a-z]{3,10}(=[0-9]{1,3})?").unwrap(),
        ) {
            let base = CacheControl::parse(&known);
            prop_assume!(!known.is_empty());
            let name = unknown.split('=').next().unwrap_or("");
            prop_assume!(!matches!(
                name,
                "no-store" | "no-cache" | "private" | "public"
                    | "max-age" | "s-maxage" | "stale-while-revalidate" | "stale-if-error"
            ));
            prop_assert_eq!(CacheControl::parse(&format!("{known}, {unknown}")), base);
        }

        /// A shared cache honours `s-maxage` ahead of `max-age`; that is what
        /// makes it a shared cache rather than a browser.
        #[test]
        fn s_maxage_outranks_max_age(shared in 0u64..3600, private_age in 0u64..3600) {
            let cc = CacheControl::parse(&format!("max-age={private_age}, s-maxage={shared}"));
            prop_assert_eq!(cc.shared_ttl(), Some(Duration::from_secs(shared)));
        }

        /// A request carrying cookies is personalised until a route says
        /// otherwise, whatever else is true about it.
        #[test]
        fn cookie_bearing_requests_bypass_unless_a_route_overrides(
            cookie in prop::string::string_regex("[a-z_]{1,8}=[a-zA-Z0-9]{0,10}").unwrap(),
            class in prop::sample::select(vec![
                RequestClass::Static,
                RequestClass::PublicDocument,
                RequestClass::PublicDynamic,
            ]),
        ) {
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::COOKIE, http::HeaderValue::from_str(&cookie).unwrap());
            let req = RequestMetadata {
                method: &http::Method::GET,
                host: "shop.example.com",
                path: "/p",
                query: None,
                headers: &headers,
            };
            let disposition = evaluate_request(&req, class, None, None, false, true, true);
            prop_assert_eq!(disposition, Disposition::Bypass(BypassReason::Cookie));
        }
    }
}
