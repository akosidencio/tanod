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
//!
//! The scheme and the public host come from the *same* hop as the address:
//! the list entry a trusted proxy wrote alongside the address the walk chose,
//! or the only entry when a proxy overwrote the header. Never simply the
//! leftmost entry, which is the one a client can prefix.
//!
//! What reaches the origin is decided here too. Tanod restates the forwarding
//! headers it concluded, removes the ones it does not interpret, and keeps
//! CDN identity headers only from trusted peers: see
//! [`STRIPPED_UPSTREAM`] and [`TRUSTED_PEER_ONLY`]. Everything an origin can
//! build a public URL from — scheme, host, port — is then derived from values
//! that are also in the cache key, so no unkeyed header can change a shared
//! response.

use crate::config::schema::{ForwardedSource, TrustedProxies};
use std::net::{IpAddr, Ipv6Addr};

/// Request headers never forwarded to the origin.
///
/// Each is either another spelling of a claim Tanod already restates
/// (`Forwarded`, the scheme aliases), or a URL-shaping value Tanod neither
/// validates nor keys on: an origin that built links or made an access
/// decision from one would let any client poison a shared response or
/// sidestep a route rule.
pub const STRIPPED_UPSTREAM: &[&str] = &[
    "forwarded",
    "x-forwarded-prefix",
    "x-forwarded-server",
    "x-original-host",
    "x-host",
    "x-original-url",
    "x-rewrite-url",
    "x-forwarded-scheme",
    "x-forwarded-ssl",
    "x-forwarded-protocol",
    "x-url-scheme",
    "front-end-https",
];

/// CDN-specific client identity headers.
///
/// Tanod does not interpret these. From a trusted peer they are the CDN's own
/// statement and pass through untouched; from anyone else they are a forged
/// identity and are removed. `X-Forwarded-For` and `X-Real-IP` carry Tanod's
/// own conclusion either way.
pub const TRUSTED_PEER_ONLY: &[&str] = &[
    "x-client-ip",
    "true-client-ip",
    "cf-connecting-ip",
    "fastly-client-ip",
    "x-cluster-client-ip",
];

/// Longest authority accepted: a 253-byte DNS name, a colon and a port.
const MAX_AUTHORITY: usize = 253 + 6;

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
    /// The public authority a trusted proxy reported, validated and
    /// lowercased. `None` unless `trusted_proxies.host` names a source, the
    /// peer is trusted, and the value is a well-formed `host[:port]`. Enters
    /// the cache key whenever it is set.
    pub forwarded_host: Option<String>,
}

impl ClientFacts {
    /// The authority the origin is told the client asked for: the believed
    /// forwarded host, else the request's own `Host` or `:authority`. `None`
    /// when neither is a well-formed authority, in which case no
    /// `X-Forwarded-Host` is sent at all.
    ///
    /// The scheme's default port is dropped, as the cache key drops it.
    /// Browsers never put a default port in `Origin`, and frameworks compare
    /// the two for CSRF checks: Next.js rejects a Server Action whose `Origin`
    /// host differs from `X-Forwarded-Host`, so `shop.example:443` would fail
    /// every one.
    pub fn public_authority(&self, request_authority: &str) -> Option<String> {
        let authority = self
            .forwarded_host
            .clone()
            .or_else(|| normalize_authority(request_authority))?;
        let default = match self.scheme {
            "https" => ":443",
            _ => ":80",
        };
        Some(match authority.strip_suffix(default) {
            // `[::1]:443` keeps its brackets; a bare `:443` never reaches here
            // because an authority needs a host.
            Some(host) if !host.is_empty() => host.to_string(),
            _ => authority,
        })
    }

    /// The public port: explicit in the authority, else the scheme's default.
    pub fn public_port(&self, authority: &str) -> u16 {
        explicit_port(authority).unwrap_or(match self.scheme {
            "https" => 443,
            _ => 80,
        })
    }
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
    host: ForwardedSource,
}

/// The hop the walk chose: its address and where it sits in the list.
#[derive(Debug, Clone, Copy)]
struct Hop {
    address: Option<IpAddr>,
    /// Position among *all* list entries, unparseable ones included, so a
    /// parallel list written by the same proxies can be read at the same place.
    index: Option<usize>,
    len: usize,
}

/// One RFC 7239 element, as written by one proxy about the request it got.
#[derive(Debug, Default)]
struct Element {
    for_: Option<IpAddr>,
    proto: Option<String>,
    host: Option<String>,
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
            host: config.host,
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
                forwarded_host: None,
            };
        }

        // Parsed only for the sources that read them. A hop is chosen only
        // from the list `client_ip` names: that is the operator's statement
        // that their proxies maintain it, which is what makes the position of
        // an entry in it mean anything.
        let reads = |source| [self.client_ip, self.scheme, self.host].contains(&source);
        let elements = if reads(ForwardedSource::Forwarded) {
            parse_forwarded(headers)
        } else {
            Vec::new()
        };
        let hop = match self.client_ip {
            ForwardedSource::XForwarded => Some(self.walk_back(&parse_x_forwarded_for(headers))),
            ForwardedSource::Forwarded => {
                Some(self.walk_back(&elements.iter().map(|e| e.for_).collect::<Vec<_>>()))
            }
            ForwardedSource::None => None,
        };
        let aligned = |source| hop.filter(|_| source == self.client_ip);

        let claimed_scheme = match self.scheme {
            ForwardedSource::None => None,
            ForwardedSource::XForwarded => {
                let values = list_values(headers, "x-forwarded-proto");
                pick(&values, aligned(ForwardedSource::XForwarded)).cloned()
            }
            ForwardedSource::Forwarded => pick(&elements, aligned(ForwardedSource::Forwarded))
                .and_then(|element| element.proto.clone()),
        };
        let claimed_host = match self.host {
            ForwardedSource::None => None,
            ForwardedSource::XForwarded => {
                let values = list_values(headers, "x-forwarded-host");
                pick(&values, aligned(ForwardedSource::XForwarded)).cloned()
            }
            ForwardedSource::Forwarded => pick(&elements, aligned(ForwardedSource::Forwarded))
                .and_then(|element| element.host.clone()),
        };

        ClientFacts {
            client_ip: hop.and_then(|hop| hop.address).or(peer),
            scheme: claimed_scheme
                .as_deref()
                .and_then(normalize_scheme)
                .unwrap_or(scheme.as_str()),
            peer_trusted: true,
            forwarded_host: claimed_host.as_deref().and_then(normalize_authority),
        }
    }

    /// Walk right to left and stop at the first address that is not itself a
    /// trusted proxy — that is the last hop a trusted proxy actually observed.
    ///
    /// Everything left of it was written by somebody we have no reason to
    /// believe. If the whole chain is trusted infrastructure, the leftmost
    /// entry is the best available answer. Entries that are not addresses
    /// (`unknown`, `_hidden`) are skipped but still counted, so the chosen
    /// position lines up with lists the same proxies wrote beside this one.
    fn walk_back(&self, chain: &[Option<IpAddr>]) -> Hop {
        let len = chain.len();
        let chosen = chain
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, address)| address.filter(|a| !self.trusts(*a)).map(|a| (index, a)))
            .or_else(|| {
                chain
                    .iter()
                    .enumerate()
                    .find_map(|(index, address)| address.map(|a| (index, a)))
            });
        Hop {
            address: chosen.map(|(_, address)| address),
            index: chosen.map(|(index, _)| index),
            len,
        }
    }
}

/// Choose the entry of a forwarding list that describes the chosen hop.
///
/// * One entry: a proxy overwrote the field, so it is that proxy's statement.
/// * As many entries as the address list: every proxy appended to both, so
///   the entry at the chosen hop's position is the one written beside it.
/// * Anything else cannot be attributed, so the rightmost entry — the one the
///   connection peer itself appended — is the only one that can be.
///
/// Never the leftmost of several: that is the entry a client prefixes.
fn pick<T>(values: &[T], hop: Option<Hop>) -> Option<&T> {
    if values.len() == 1 {
        return values.first();
    }
    if let Some(Hop {
        index: Some(index),
        len,
        ..
    }) = hop
        && len == values.len()
    {
        return values.get(index);
    }
    values.last()
}

/// Every comma-separated entry of a list-valued header, across repeated
/// fields, trimmed. Empty entries are kept so positions stay aligned.
fn list_values(headers: &http::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .flat_map(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .split(',')
                .map(|entry| entry.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
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

/// Every entry in every `X-Forwarded-For` header, left to right.
///
/// Repeated headers are concatenated in order, which is what RFC 9110 says a
/// list-valued field means and what an HTTP/2 client naturally produces.
/// Entries that are not addresses become `None` rather than aborting the
/// parse: a proxy that writes `unknown` should not make the rest of the chain
/// unreadable.
fn parse_x_forwarded_for(headers: &http::HeaderMap) -> Vec<Option<IpAddr>> {
    list_values(headers, "x-forwarded-for")
        .iter()
        .map(|entry| parse_address(entry))
        .collect()
}

/// Every RFC 7239 `Forwarded` element, left to right.
fn parse_forwarded(headers: &http::HeaderMap) -> Vec<Element> {
    list_values(headers, "forwarded")
        .iter()
        .map(|element| Element {
            for_: param(element, "for").and_then(parse_address),
            proto: param(element, "proto").map(str::to_string),
            host: param(element, "host").map(str::to_string),
        })
        .collect()
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

/// Validate and lowercase one `host[:port]` authority.
///
/// Strict on purpose: the result is sent to the origin, which builds URLs from
/// it, and enters the cache key. A registered name of letters, digits, `-`,
/// `_` and dots, an IPv4 address, or a bracketed IPv6 literal, optionally with
/// a non-zero port. No userinfo, no path, no whitespace, no list.
pub fn normalize_authority(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_AUTHORITY || !raw.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (literal, after) = rest.split_once(']')?;
        literal.parse::<Ipv6Addr>().ok()?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (&raw[..literal.len() + 2], port)
    } else {
        match raw.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (raw, None),
        }
    };
    if !host.starts_with('[') && !valid_reg_name(host) {
        return None;
    }
    if let Some(port) = port
        && (port.is_empty()
            || port.len() > 5
            || !port.bytes().all(|b| b.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|p| p != 0))
    {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

/// A DNS-shaped name or a dotted IPv4 address: dot-separated labels of 1–63
/// letters, digits, `-` or `_`, with no empty label.
fn valid_reg_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The port written in an authority already accepted by
/// [`normalize_authority`], or a `Host` as received.
fn explicit_port(authority: &str) -> Option<u16> {
    let after_host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']')?.1,
        None => authority.rsplit_once(':').map_or("", |(_, port)| port),
    };
    after_host
        .strip_prefix(':')
        .unwrap_or(after_host)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn policy(from: &[&str], client_ip: ForwardedSource, scheme: ForwardedSource) -> TrustPolicy {
        with_host(from, client_ip, scheme, ForwardedSource::None)
    }

    fn with_host(
        from: &[&str],
        client_ip: ForwardedSource,
        scheme: ForwardedSource,
        host: ForwardedSource,
    ) -> TrustPolicy {
        TrustPolicy::build(&TrustedProxies {
            from: from.iter().map(|s| (*s).to_string()).collect(),
            client_ip,
            scheme,
            host,
        })
        .unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
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
        // The element that names the chosen hop is the one written by the
        // proxy that saw the client, so it knows what the client dialled.
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
        headers.insert("x-forwarded-proto", HeaderValue::from_static(" HtTpS "));
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

    // ------------------------------------------- hop-aligned scheme and host

    /// A client prefixes its own claims to a chain the trusted proxy appends
    /// to. The address walk already ignores the prefix; the scheme and host
    /// must come from the same hop, not from the leftmost entry.
    #[test]
    fn a_spoofed_prefix_cannot_choose_the_scheme_or_host() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[
                ("x-forwarded-for", "9.9.9.9, 203.0.113.7, 10.0.0.9"),
                ("x-forwarded-proto", "http, https, http"),
                (
                    "x-forwarded-host",
                    "evil.example, shop.example, internal.lb",
                ),
            ]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.client_ip, Some(ip("203.0.113.7")));
        assert_eq!(facts.scheme, "https");
        assert_eq!(facts.forwarded_host.as_deref(), Some("shop.example"));
    }

    #[test]
    fn a_single_overwritten_value_is_used_whatever_the_chain_length() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[
                ("x-forwarded-for", "203.0.113.7, 10.0.0.8, 10.0.0.9"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "Shop.Example:8443"),
            ]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.scheme, "https");
        assert_eq!(facts.forwarded_host.as_deref(), Some("shop.example:8443"));
    }

    /// Lists that cannot be lined up with the address chain fall back to the
    /// entry the connection peer appended, never the client's.
    #[test]
    fn an_unalignable_list_uses_the_peers_own_entry() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[
                ("x-forwarded-for", "1.1.1.1, 2.2.2.2, 203.0.113.7"),
                ("x-forwarded-proto", "https, http"),
                ("x-forwarded-host", "evil.example, shop.example"),
            ]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.scheme, "http");
        assert_eq!(facts.forwarded_host.as_deref(), Some("shop.example"));

        // Without an address list to align against, the same rule applies.
        let scheme_only = policy_scheme_only();
        let facts = scheme_only.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[("x-forwarded-proto", "https, http")]),
            ListenerScheme::Https,
        );
        assert_eq!(facts.scheme, "http");
    }

    fn policy_scheme_only() -> TrustPolicy {
        policy(
            &["10.0.0.0/8"],
            ForwardedSource::None,
            ForwardedSource::XForwarded,
        )
    }

    /// Two trusted hops, each appending to every list: CDN then load balancer.
    #[test]
    fn multiple_trusted_proxies_resolve_to_the_edge_hop() {
        let policy = with_host(
            &["10.0.0.0/8", "198.51.100.0/24"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[
                ("x-forwarded-for", "203.0.113.7, 198.51.100.20"),
                ("x-forwarded-proto", "https, http"),
                ("x-forwarded-host", "shop.example, origin.internal"),
            ]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.client_ip, Some(ip("203.0.113.7")));
        assert_eq!(facts.scheme, "https", "TLS ended at the CDN, not here");
        assert_eq!(facts.forwarded_host.as_deref(), Some("shop.example"));
    }

    #[test]
    fn rfc_7239_takes_proto_and_host_from_the_chosen_element() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::Forwarded,
            ForwardedSource::Forwarded,
            ForwardedSource::Forwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[(
                "forwarded",
                r#"for=9.9.9.9;proto=http;host=evil.example, for="[2001:db8::7]";proto=https;host=shop.example, for=10.0.0.9;proto=http;host=lb.internal"#,
            )]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.client_ip, Some(ip("2001:db8::7")));
        assert_eq!(facts.scheme, "https");
        assert_eq!(facts.forwarded_host.as_deref(), Some("shop.example"));
    }

    #[test]
    fn the_host_is_not_read_unless_configured() {
        let policy = policy(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("10.0.0.4")),
            &headers(&[("x-forwarded-host", "shop.example")]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.forwarded_host, None);
        // And the default configuration is that.
        assert_eq!(TrustedProxies::default().host, ForwardedSource::None);
    }

    #[test]
    fn an_untrusted_peer_cannot_set_the_host() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
            ForwardedSource::XForwarded,
        );
        let facts = policy.resolve(
            Some(ip("203.0.113.7")),
            &headers(&[("x-forwarded-host", "evil.example")]),
            ListenerScheme::Http,
        );
        assert_eq!(facts.forwarded_host, None);
    }

    #[test]
    fn malformed_hosts_are_ignored_not_forwarded() {
        let policy = with_host(
            &["10.0.0.0/8"],
            ForwardedSource::None,
            ForwardedSource::None,
            ForwardedSource::XForwarded,
        );
        for claim in [
            "",
            "user@shop.example",
            "shop.example/path",
            "shop.example:0",
            "shop.example:70000",
            "shop.example:",
            "shop..example",
            ".shop.example",
            "shop example",
            "[not-v6]",
            "[::1",
            "shop.example:443:1",
            "shöp.example",
            "shop\texample",
        ] {
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(claim) {
                headers.insert("x-forwarded-host", value);
            } else {
                headers.insert(
                    "x-forwarded-host",
                    HeaderValue::from_bytes(claim.as_bytes()).unwrap(),
                );
            }
            let facts = policy.resolve(Some(ip("10.0.0.4")), &headers, ListenerScheme::Http);
            assert_eq!(facts.forwarded_host, None, "`{claim}` was believed");
        }
    }

    #[test]
    fn well_formed_authorities_normalise() {
        for (raw, expected) in [
            ("Shop.Example", "shop.example"),
            ("shop.example:8443", "shop.example:8443"),
            ("203.0.113.7:80", "203.0.113.7:80"),
            ("[2001:DB8::1]:443", "[2001:db8::1]:443"),
            ("[::1]", "[::1]"),
            ("internal_svc.local", "internal_svc.local"),
            (" localhost:3000 ", "localhost:3000"),
        ] {
            assert_eq!(normalize_authority(raw).as_deref(), Some(expected), "{raw}");
        }
    }

    #[test]
    fn the_public_port_is_explicit_or_the_schemes_default() {
        let mut facts = ClientFacts {
            client_ip: None,
            scheme: "https",
            peer_trusted: false,
            forwarded_host: None,
        };
        assert_eq!(facts.public_port("shop.example"), 443);
        assert_eq!(facts.public_port("shop.example:8443"), 8443);
        assert_eq!(facts.public_port("[::1]:9000"), 9000);
        assert_eq!(facts.public_port("[::1]"), 443);
        facts.scheme = "http";
        assert_eq!(facts.public_port("shop.example"), 80);
        assert_eq!(
            facts.public_authority("Shop.Example:8080").as_deref(),
            Some("shop.example:8080")
        );
        assert_eq!(facts.public_authority(""), None);
        facts.forwarded_host = Some("public.example".into());
        assert_eq!(
            facts.public_authority("internal:3000").as_deref(),
            Some("public.example")
        );
    }

    /// A default port in the stated authority would never match a browser's
    /// `Origin`, which omits it.
    #[test]
    fn the_default_port_is_dropped_for_the_effective_scheme_only() {
        let facts = |scheme: &'static str, host: Option<&str>| ClientFacts {
            client_ip: None,
            scheme,
            peer_trusted: true,
            forwarded_host: host.map(str::to_string),
        };
        let https = facts("https", Some("shop.example:443"));
        assert_eq!(https.public_authority("x").as_deref(), Some("shop.example"));
        assert_eq!(https.public_port("shop.example"), 443);
        let http = facts("http", None);
        assert_eq!(
            http.public_authority("Shop.Example:80").as_deref(),
            Some("shop.example")
        );
        assert_eq!(
            http.public_authority("shop.example:443").as_deref(),
            Some("shop.example:443"),
            "443 is not http's default"
        );
        assert_eq!(
            facts("https", Some("[2001:db8::1]:443"))
                .public_authority("x")
                .as_deref(),
            Some("[2001:db8::1]")
        );
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
            host: ForwardedSource::XForwarded,
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
            if let Ok(value) = HeaderValue::from_str(&chain) {
                headers.insert("x-forwarded-host", value);
            }
            let peer: IpAddr = "203.0.113.7".parse().unwrap();
            let facts = policy.resolve(Some(peer), &headers, ListenerScheme::Https);
            prop_assert_eq!(facts.client_ip, Some(peer));
            prop_assert_eq!(facts.scheme, "https");
            prop_assert_eq!(facts.forwarded_host, None);
        }

        /// Whatever a trusted peer claims, a believed host is a bare
        /// authority: no separators that could smuggle a path, list or
        /// userinfo into a URL the origin builds, and no upper case to split
        /// the cache key.
        #[test]
        fn a_believed_host_is_always_a_plain_authority(
            claim in prop::string::string_regex("[ -~]{0,60}").unwrap(),
        ) {
            let policy = policy(&["10.0.0.0/8"]);
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&claim) {
                headers.insert("x-forwarded-host", value);
            }
            let facts = policy.resolve(
                Some("10.0.0.4".parse().unwrap()),
                &headers,
                ListenerScheme::Http,
            );
            if let Some(host) = facts.forwarded_host {
                prop_assert!(!host.is_empty());
                prop_assert!(host.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')));
                prop_assert_eq!(normalize_authority(&host), Some(host.clone()));
            }
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
                host: ForwardedSource::Forwarded,
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
