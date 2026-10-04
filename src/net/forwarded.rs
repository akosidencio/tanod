//! Who is the client, and what scheme did they use?
//!
//! Both questions have an easy wrong answer. `X-Forwarded-For`,
//! `X-Forwarded-Proto` and RFC 7239 `Forwarded` are all set by whoever spoke
//! to Tanod last, and on a public listener that is the client themselves.
//! Reading them unconditionally hands an attacker two things:
//!
//! * **A cache partition they control.** The scheme is part of the cache key.
//!   A client that can set `X-Forwarded-Proto: ftp` can mint an unlimited
//!   number of distinct keys for one URL, which is a render per probe — the
//!   exact origin-work amplification this project exists to stop.
//! * **A forged identity in the audit trail**, and, if the origin trusts the
//!   `X-Forwarded-For` Tanod passes on, in the origin's own logs and
//!   rate limits.
//!
//! So the rule is: a forwarded header is read only when the *connection peer*
//! is inside a configured trusted block. Nothing is trusted by default, which
//! means an unconfigured Tanod cannot be lied to — it just reports the peer.
//!
//! The other half is the hop walk. A chain like
//! `X-Forwarded-For: 9.9.9.9, 203.0.113.7, 10.0.0.4` is partly attacker-written:
//! everything left of the first address a trusted proxy appended is whatever
//! the client sent. Walking from the right and stopping at the first address
//! that is *not* itself trusted is what makes the result the address a trusted
//! proxy observed rather than one the client chose.

use crate::config::schema::{ForwardedSource, TrustedProxies};
use std::net::IpAddr;

/// The scheme a listener speaks before any header is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerScheme {
    Http,
    Https,
}

impl ListenerScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            ListenerScheme::Http => "http",
            ListenerScheme::Https => "https",
        }
    }
}

/// What Tanod concluded about the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFacts {
    /// The address to log, and to send upstream as `X-Forwarded-For`.
    pub client_ip: Option<IpAddr>,
    /// `http` or `https`, never anything else. Enters the cache key.
    pub scheme: &'static str,
    /// Was the connection peer trusted to describe the client at all?
    pub peer_trusted: bool,
}

/// One CIDR block, or a single host when no prefix length is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// `10.0.0.0/8`, `192.168.1.7`, `2001:db8::/32`, `::1`.
    ///
    /// A bare address is a single host: `/32` for IPv4, `/128` for IPv6.
    /// Host bits outside the prefix are ignored rather than rejected, so
    /// `10.1.2.3/8` means the same block as `10.0.0.0/8` — this is a trust
    /// list, and refusing to boot over a sloppy but unambiguous entry helps
    /// nobody.
    pub fn parse(raw: &str) -> Result<Cidr, String> {
        let raw = raw.trim();
        let (address, prefix) = match raw.rsplit_once('/') {
            Some((address, len)) => {
                let prefix: u8 = len
                    .parse()
                    .map_err(|_| format!("`{raw}` has a prefix length that is not a number"))?;
                (address, Some(prefix))
            }
            None => (raw, None),
        };
        // A bracketed IPv6 literal is what people copy out of a URL.
        let address = address
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(address);
        let network: IpAddr = address
            .parse()
            .map_err(|_| format!("`{raw}` is not an IP address or CIDR block"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return Err(format!(
                "`{raw}` has a /{prefix} prefix, but the maximum for this address family is /{max}"
            ));
        }
        Ok(Cidr { network, prefix })
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                prefix_eq(&network.octets(), &address.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                prefix_eq(&network.octets(), &address.octets(), self.prefix)
            }
            // An IPv4-mapped IPv6 peer (`::ffff:10.0.0.1`) is how a dual-stack
            // listener reports an IPv4 connection. Comparing it against an
            // IPv4 block as-is would say "not trusted" for a proxy that is.
            (IpAddr::V4(_), IpAddr::V6(address)) => match address.to_ipv4_mapped() {
                Some(mapped) => self.contains(IpAddr::V4(mapped)),
                None => false,
            },
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// Do two addresses agree on their first `prefix` bits?
fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let whole = (prefix / 8) as usize;
    if a[..whole] != b[..whole] {
        return false;
    }
    let leftover = prefix % 8;
    if leftover == 0 {
        return true;
    }
    // `!0u8 << (8 - leftover)` keeps the high `leftover` bits.
    let mask = !0u8 << (8 - leftover);
    (a[whole] & mask) == (b[whole] & mask)
}

/// The compiled `server.trusted_proxies` block.
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    blocks: Vec<Cidr>,
    client_ip: ForwardedSource,
    scheme: ForwardedSource,
}

impl TrustPolicy {
    pub fn build(config: &TrustedProxies) -> Result<TrustPolicy, String> {
        let blocks = config
            .from
            .iter()
            .map(|raw| Cidr::parse(raw))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TrustPolicy {
            blocks,
            client_ip: config.client_ip,
            scheme: config.scheme,
        })
    }

    pub fn trusts(&self, address: IpAddr) -> bool {
        self.blocks.iter().any(|block| block.contains(address))
    }

    /// Resolve the client address and scheme for one request.
    ///
    /// `peer` is the address of the TCP connection — the only input here that
    /// cannot be forged. `scheme` is what the listener itself speaks, and is
    /// the answer whenever no trusted proxy said otherwise.
    pub fn resolve(
        &self,
        peer: Option<IpAddr>,
        headers: &http::HeaderMap,
        scheme: ListenerScheme,
    ) -> ClientFacts {
        let trusted = peer.is_some_and(|address| self.trusts(address));
        if !trusted {
            return ClientFacts {
                client_ip: peer,
                scheme: scheme.as_str(),
                peer_trusted: false,
            };
        }
        ClientFacts {
            client_ip: self.forwarded_client(headers).or(peer),
            scheme: self.forwarded_scheme(headers).unwrap_or(scheme.as_str()),
            peer_trusted: true,
        }
    }

    fn forwarded_client(&self, headers: &http::HeaderMap) -> Option<IpAddr> {
        let chain: Vec<IpAddr> = match self.client_ip {
            ForwardedSource::None => return None,
            ForwardedSource::XForwarded => parse_x_forwarded_for(headers),
            ForwardedSource::Forwarded => parse_forwarded_for(headers),
        };
        self.walk_back(&chain)
    }

    /// Walk right to left and stop at the first address that is not itself a
    /// trusted proxy — that is the last hop a trusted proxy actually observed.
    ///
    /// Everything left of it was written by somebody we have no reason to
    /// believe. If the whole chain is trusted infrastructure, the leftmost
    /// entry is the best available answer.
    fn walk_back(&self, chain: &[IpAddr]) -> Option<IpAddr> {
        for address in chain.iter().rev() {
            if !self.trusts(*address) {
                return Some(*address);
            }
        }
        chain.first().copied()
    }

    fn forwarded_scheme(&self, headers: &http::HeaderMap) -> Option<&'static str> {
        let raw = match self.scheme {
            ForwardedSource::None => return None,
            ForwardedSource::XForwarded => first_token(headers.get("x-forwarded-proto")?),
            ForwardedSource::Forwarded => {
                // The leftmost element is the hop closest to the client, and
                // therefore the one that knows what the client dialled.
                let element = first_element(headers.get("forwarded")?)?;
                param(&element, "proto")?.to_string()
            }
        };
        normalize_scheme(&raw)
    }
}

/// Only two schemes exist as far as the cache key is concerned.
///
/// Anything else — `ftp`, `httpss`, an empty value, a 4KiB string — is not a
/// scheme Tanod serves, and accepting it would let a client mint cache keys
/// by inventing scheme names. Falling back to the listener's own scheme is
/// both correct and unforgeable.
fn normalize_scheme(raw: &str) -> Option<&'static str> {
    if raw.eq_ignore_ascii_case("https") {
        Some("https")
    } else if raw.eq_ignore_ascii_case("http") {
        Some("http")
    } else {
        None
    }
}

/// `X-Forwarded-Proto: https, http` — take the leftmost, which is the hop
/// closest to the client.
fn first_token(value: &http::HeaderValue) -> String {
    let text = String::from_utf8_lossy(value.as_bytes());
    text.split(',').next().unwrap_or("").trim().to_string()
}

/// Every address in every `X-Forwarded-For` header, left to right.
///
/// Repeated headers are concatenated in order, which is what RFC 9110 says a
/// list-valued field means and what an HTTP/2 client naturally produces.
/// Entries that are not addresses are dropped rather than aborting the parse:
/// a proxy that writes `unknown` should not make the rest of the chain
/// unreadable.
fn parse_x_forwarded_for(headers: &http::HeaderMap) -> Vec<IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .flat_map(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .split(',')
                .filter_map(parse_address)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The `for=` parameter of every RFC 7239 `Forwarded` element, left to right.
fn parse_forwarded_for(headers: &http::HeaderMap) -> Vec<IpAddr> {
    headers
        .get_all("forwarded")
        .iter()
        .flat_map(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .split(',')
                .filter_map(|element| param(element, "for").and_then(parse_address))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn first_element(value: &http::HeaderValue) -> Option<String> {
    let text = String::from_utf8_lossy(value.as_bytes());
    text.split(',').next().map(str::to_string)
}

/// Pull one `;`-separated parameter out of a `Forwarded` element.
///
/// Values may be quoted (`for="[2001:db8::1]:443"`) and names are
/// case-insensitive, both per RFC 7239 §4.
fn param<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    element.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().trim_matches('"'))
    })
}

/// Parse one chain entry into an address.
///
/// Handles the four shapes that turn up in the wild: a bare address, an
/// address with a port, a bracketed IPv6 literal, and a bracketed IPv6
/// literal with a port. RFC 7239 obfuscated identifiers (`_hidden`) and the
/// `unknown` placeholder parse as nothing, which is the correct answer —
/// they name a hop whose address was deliberately withheld.
fn parse_address(raw: &str) -> Option<IpAddr> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    // `[2001:db8::1]` or `[2001:db8::1]:443`
    if let Some(rest) = raw.strip_prefix('[') {
        let (inside, _) = rest.split_once(']')?;
        return inside.parse().ok();
    }
    if let Ok(address) = raw.parse::<IpAddr>() {
        return Some(address);
    }
    // A bare `1.2.3.4:5678`. An unbracketed IPv6 with a port is ambiguous and
    // nothing writes it, so a colon here only ever means IPv4-with-port.
    let (host, port) = raw.rsplit_once(':')?;
    if port.parse::<u16>().is_err() {
        return None;
    }
    host.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn policy(from: &[&str], client_ip: ForwardedSource, scheme: ForwardedSource) -> TrustPolicy {
        TrustPolicy::build(&TrustedProxies {
            from: from.iter().map(|s| (*s).to_string()).collect(),
            client_ip,
            scheme,
        })
        .unwrap()
    }

    fn xff(policy: &TrustPolicy, peer: &str, chain: &str) -> Option<IpAddr> {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(chain).unwrap());
        policy
            .resolve(Some(ip(peer)), &headers, ListenerScheme::Http)
            .client_ip
    }

    #[test]
    fn cidr_matches_only_inside_the_prefix() {
        let block = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(block.contains(ip("10.255.255.254")));
        assert!(!block.contains(ip("11.0.0.1")));

        // A prefix that does not land on a byte boundary is the case a
        // byte-wise comparison gets wrong.
        let block = Cidr::parse("192.168.1.0/28").unwrap();
        assert!(block.contains(ip("192.168.1.15")));
        assert!(!block.contains(ip("192.168.1.16")));
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        let block = Cidr::parse("203.0.113.7").unwrap();
        assert!(block.contains(ip("203.0.113.7")));
        assert!(!block.contains(ip("203.0.113.8")));
    }

    #[test]
    fn ipv6_blocks_and_bracketed_literals_parse() {
        let block = Cidr::parse("2001:db8::/32").unwrap();
        assert!(block.contains(ip("2001:db8:1234::1")));
        assert!(!block.contains(ip("2001:db9::1")));
        assert!(Cidr::parse("[::1]").unwrap().contains(ip("::1")));
    }

    #[test]
    fn an_ipv4_mapped_peer_matches_an_ipv4_block() {
        // A dual-stack listener reports an IPv4 connection this way. Reading
        // it as "not in 10.0.0.0/8" would silently stop trusting a proxy that
        // is in fact trusted, and the symptom is every client address in the
        // logs turning into the load balancer's.
        let block = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(block.contains(ip("::ffff:10.1.2.3")));
        assert!(!block.contains(ip("::ffff:11.1.2.3")));
    }

    #[test]
    fn nonsense_cidrs_are_rejected_at_parse_time() {
        assert!(Cidr::parse("not-an-address").is_err());
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("2001:db8::/129").is_err());
        assert!(Cidr::parse("10.0.0.0/eight").is_err());
    }

    #[test]
    fn an_untrusted_peer_is_the_client_whatever_it_claims() {
        let policy = policy(
            &[],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));

        let facts = policy.resolve(Some(ip("198.51.100.9")), &headers, ListenerScheme::Http);
        assert_eq!(facts.client_ip, Some(ip("198.51.100.9")));
        assert_eq!(facts.scheme, "http");
        assert!(!facts.peer_trusted);
    }

    #[test]
    fn a_trusted_peer_is_believed() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));

        let facts = policy.resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http);
        assert_eq!(facts.client_ip, Some(ip("203.0.113.7")));
        assert_eq!(facts.scheme, "https");
        assert!(facts.peer_trusted);
    }

    #[test]
    fn the_walk_stops_at_the_first_untrusted_hop_from_the_right() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::None,
        );
        // Everything left of 203.0.113.7 is whatever the client sent. Reading
        // the leftmost entry — the naive implementation — returns 9.9.9.9,
        // an address the client chose.
        assert_eq!(
            xff(&policy, "10.0.0.4", "9.9.9.9, 203.0.113.7, 10.0.0.9"),
            Some(ip("203.0.113.7"))
        );
    }

    #[test]
    fn a_chain_that_is_entirely_trusted_falls_back_to_the_leftmost() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::None,
        );
        assert_eq!(
            xff(&policy, "10.0.0.4", "10.0.0.1, 10.0.0.2"),
            Some(ip("10.0.0.1"))
        );
    }

    #[test]
    fn repeated_headers_are_one_chain_in_order() {
        // HTTP/2 clients routinely split a list-valued field across frames.
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::None,
        );
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("9.9.9.9"));
        headers.append("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        headers.append("x-forwarded-for", HeaderValue::from_static("10.0.0.9"));
        assert_eq!(
            policy
                .resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http)
                .client_ip,
            Some(ip("203.0.113.7"))
        );
    }

    #[test]
    fn entries_with_ports_and_brackets_still_parse() {
        assert_eq!(parse_address("203.0.113.7:44321"), Some(ip("203.0.113.7")));
        assert_eq!(parse_address("[2001:db8::1]"), Some(ip("2001:db8::1")));
        assert_eq!(parse_address("[2001:db8::1]:443"), Some(ip("2001:db8::1")));
        assert_eq!(parse_address("2001:db8::1"), Some(ip("2001:db8::1")));
    }

    #[test]
    fn obfuscated_and_unknown_hops_are_skipped_not_fatal() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::None,
        );
        assert_eq!(
            xff(&policy, "10.0.0.4", "unknown, 203.0.113.7, _hidden"),
            Some(ip("203.0.113.7"))
        );
    }

    #[test]
    fn rfc_7239_forwarded_is_parsed_when_selected() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::Forwarded,
            ForwardedSource::Forwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            HeaderValue::from_static(
                r#"for="203.0.113.7";proto=https;by=10.0.0.4, for=10.0.0.9;proto=http"#,
            ),
        );
        let facts = policy.resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http);
        assert_eq!(facts.client_ip, Some(ip("203.0.113.7")));
        // The leftmost element is the hop that knows what the client dialled.
        assert_eq!(facts.scheme, "https");
    }

    #[test]
    fn x_forwarded_headers_are_ignored_when_forwarded_is_selected() {
        // Mixing the two is how a "trusted" deployment ends up reading a
        // header its proxy never sanitises.
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::Forwarded,
            ForwardedSource::Forwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let facts = policy.resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http);
        assert_eq!(facts.client_ip, Some(ip("10.0.0.4")));
        assert_eq!(facts.scheme, "http");
    }

    #[test]
    fn an_invented_scheme_never_reaches_the_cache_key() {
        // The whole point of normalising: a client behind a trusted proxy that
        // forwards the header verbatim could otherwise mint one cache key per
        // scheme string, and therefore one origin render per probe.
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::None,
            ForwardedSource::XForwarded,
        );
        for claim in ["ftp", "HTTPS\u{0}", "", "javascript", "https ", "httpss"] {
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(claim) {
                headers.insert("x-forwarded-proto", value);
            }
            let scheme = policy
                .resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http)
                .scheme;
            assert!(
                scheme == "http" || scheme == "https",
                "`{claim}` produced scheme `{scheme}`"
            );
        }
    }

    #[test]
    fn case_and_whitespace_do_not_change_the_scheme() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::None,
            ForwardedSource::XForwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-proto",
            HeaderValue::from_static(" HtTpS , http"),
        );
        assert_eq!(
            policy
                .resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http)
                .scheme,
            "https"
        );
    }

    #[test]
    fn a_tls_listener_reports_https_with_no_headers_at_all() {
        let policy = policy(&[], ForwardedSource::None, ForwardedSource::None);
        let facts = policy.resolve(
            Some(ip("203.0.113.7")),
            &HeaderMap::new(),
            ListenerScheme::Https,
        );
        assert_eq!(facts.scheme, "https");
    }

    #[test]
    fn a_peerless_connection_does_not_become_trusted() {
        // A Unix socket has no IP. Treating "no address" as "matches nothing"
        // is the safe reading; the alternative is a trust check that passes
        // by accident.
        let policy = policy(
            &["0.0.0.0/0"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let facts = policy.resolve(None, &headers, ListenerScheme::Http);
        assert!(!facts.peer_trusted);
        assert_eq!(facts.client_ip, None);
    }
}

/// Properties over inputs no proxy would ever write on purpose.
///
/// Everything here is reachable from the network by anyone, and two of the
/// three properties are safety properties rather than correctness ones: the
/// scheme must stay inside a two-element set no matter what arrives, and an
/// untrusted peer must never be able to change the answer.
#[cfg(test)]
mod proptests {
    use super::*;
    use http::{HeaderMap, HeaderValue};
    use proptest::prelude::*;

    fn policy(from: &[&str]) -> TrustPolicy {
        TrustPolicy::build(&TrustedProxies {
            from: from.iter().map(|s| (*s).to_string()).collect(),
            client_ip: ForwardedSource::XForwarded,
            scheme: ForwardedSource::XForwarded,
        })
        .unwrap()
    }

    proptest! {
        /// The cache key carries the scheme. If an arbitrary header value can
        /// produce an arbitrary scheme string, an attacker owns a key
        /// dimension and can force a fresh render per request.
        #[test]
        fn the_scheme_is_always_http_or_https(
            claim in prop::string::string_regex("[ -~]{0,40}").unwrap(),
            trusted in any::<bool>(),
        ) {
            let policy = policy(if trusted { &["10.0.0.0/8"] } else { &[] });
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&claim) {
                headers.insert("x-forwarded-proto", value);
            }
            let facts = policy.resolve(
                Some("10.0.0.4".parse().unwrap()),
                &headers,
                ListenerScheme::Http,
            );
            prop_assert!(matches!(facts.scheme, "http" | "https"));
        }

        /// Nothing an untrusted peer sends can change what Tanod concludes.
        #[test]
        fn an_untrusted_peer_cannot_move_the_answer(
            chain in prop::string::string_regex("[0-9a-f.:, \\[\\]]{0,60}").unwrap(),
            proto in prop::string::string_regex("[a-z]{0,10}").unwrap(),
        ) {
            let policy = policy(&["10.0.0.0/8"]);
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&chain) {
                headers.insert("x-forwarded-for", value);
            }
            if let Ok(value) = HeaderValue::from_str(&proto) {
                headers.insert("x-forwarded-proto", value);
            }
            let peer: IpAddr = "203.0.113.7".parse().unwrap();
            let facts = policy.resolve(Some(peer), &headers, ListenerScheme::Https);
            prop_assert_eq!(facts.client_ip, Some(peer));
            prop_assert_eq!(facts.scheme, "https");
        }

        /// Parsing must be total. A panic here is a denial of service that
        /// costs one request to trigger.
        #[test]
        fn parsing_never_panics(
            chain in prop::string::string_regex("[ -~]{0,80}").unwrap(),
            forwarded in prop::string::string_regex("[ -~]{0,80}").unwrap(),
        ) {
            let policy = TrustPolicy::build(&TrustedProxies {
                from: vec!["10.0.0.0/8".into()],
                client_ip: ForwardedSource::Forwarded,
                scheme: ForwardedSource::Forwarded,
            })
            .unwrap();
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&chain) {
                headers.insert("x-forwarded-for", value);
            }
            if let Ok(value) = HeaderValue::from_str(&forwarded) {
                headers.insert("forwarded", value);
            }
            let _ = policy.resolve(Some("10.0.0.4".parse().unwrap()), &headers, ListenerScheme::Http);
        }
    }
}
