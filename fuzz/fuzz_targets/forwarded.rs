#![no_main]
//! Forwarded-header resolution.
//!
//! Two reasons this belongs here rather than only in unit tests.
//!
//! First, the inputs are the most directly attacker-controlled bytes Tanod
//! reads. `X-Forwarded-For`, `X-Forwarded-Proto` and RFC 7239 `Forwarded` are
//! set by whoever spoke to the proxy last, and on a public listener that is
//! the client. A panic here is a denial of service that costs one request.
//!
//! Second, the properties are safety properties rather than correctness ones,
//! so they can be stated as invariants that must hold for *every* input rather
//! than as expected outputs for chosen ones:
//!
//! * the resolved scheme is always `http` or `https`, because it is a cache
//!   key component and anything else hands an attacker an unbounded key space
//!   — a fresh origin render per invented scheme string;
//! * an untrusted peer never moves the answer, whatever it sends.

use tanod::config::schema::{ForwardedSource, TrustedProxies};
use tanod::net::forwarded::{ListenerScheme, TrustPolicy};
use libfuzzer_sys::fuzz_target;

#[derive(arbitrary::Arbitrary, Debug)]
struct Input {
    x_forwarded_for: String,
    x_forwarded_proto: String,
    forwarded: String,
    /// Which header family the policy is configured to read.
    use_rfc7239: bool,
    /// Whether the connection peer is inside the trusted block.
    peer_trusted: bool,
    tls_listener: bool,
}

fuzz_target!(|input: Input| {
    let source = if input.use_rfc7239 {
        ForwardedSource::Forwarded
    } else {
        ForwardedSource::XForwarded
    };
    let policy = TrustPolicy::build(&TrustedProxies {
        from: vec!["10.0.0.0/8".into()],
        client_ip: source,
        scheme: source,
    })
    .expect("a fixed, valid trust block");

    let mut headers = http::HeaderMap::new();
    for (name, value) in [
        ("x-forwarded-for", &input.x_forwarded_for),
        ("x-forwarded-proto", &input.x_forwarded_proto),
        ("forwarded", &input.forwarded),
    ] {
        // Only values `http` itself accepts can reach the proxy, so only those
        // are worth generating. Everything else is rejected before Tanod is
        // ever asked.
        if let Ok(value) = http::HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    }

    let listener = if input.tls_listener {
        ListenerScheme::Https
    } else {
        ListenerScheme::Http
    };
    // `10.0.0.4` is inside the trusted block; `203.0.113.7` is not.
    let peer: std::net::IpAddr = if input.peer_trusted {
        "10.0.0.4".parse().unwrap()
    } else {
        "203.0.113.7".parse().unwrap()
    };

    let facts = policy.resolve(Some(peer), &headers, listener);

    // The scheme is a cache-key component. If an arbitrary header value can
    // produce an arbitrary scheme, the client owns a key dimension.
    assert!(
        matches!(facts.scheme, "http" | "https"),
        "resolved scheme `{}` is not a scheme",
        facts.scheme
    );

    // An untrusted peer is the client, whatever it claims to be.
    if !input.peer_trusted {
        assert!(!facts.peer_trusted);
        assert_eq!(facts.client_ip, Some(peer));
        assert_eq!(facts.scheme, listener.as_str());
    }

    // Resolution is a pure function of its inputs: the same request must not
    // resolve two ways, or one request could be logged as one client and keyed
    // as another.
    let again = policy.resolve(Some(peer), &headers, listener);
    assert_eq!(facts, again);
});
