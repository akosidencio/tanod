//! W3C Trace Context: correlation for every request, whether or not spans are
//! exported anywhere.
//!
//! Two things are separable and only one of them is optional.
//!
//! **Correlation** is unconditional. Every request gets a trace id and a span
//! id, both appear in the access log, and the id Tanod concluded is what
//! reaches the origin in `traceparent`. That alone is what turns "the origin
//! logged a slow render at 12:04" into "*this* request, which Tanod shed a
//! sibling of, and here is its route and its cache status".
//!
//! **Export** is configuration. See [`super::otlp`].
//!
//! # An inbound `traceparent` is a claim
//!
//! Anyone can send one, and believing it means anyone on the internet can
//! write into your tracing backend under a trace id of their choosing, or
//! attach their requests to someone else's trace. The default is to ignore it.
//! Unlike `X-Forwarded-For`, trace context has no hop chain Tanod can inspect
//! to distinguish a header a trusted proxy created from one it merely passed
//! through. `from_trusted_proxies` is therefore safe only when every trusted
//! proxy strips or replaces client-supplied trace headers.
//!
//! Note what that does *not* break. When an inbound context is ignored,
//! Tanod still traces the request — it simply starts a new trace rather than
//! joining one. A dropped request is never the failure mode.

use crate::config::schema::{Sample, SampleMode, TrustIncoming};
use std::sync::atomic::{AtomicU64, Ordering};

/// The header carrying an inbound trace context, lowercased because that is
/// how HTTP/2 delivers it and how `http::HeaderMap` indexes either way.
pub const TRACEPARENT: &str = "traceparent";
/// Vendor state travelling alongside it. Forwarded verbatim when believed,
/// because its contents belong to systems Tanod knows nothing about.
pub const TRACESTATE: &str = "tracestate";

/// The longest `tracestate` Tanod will carry.
///
/// The specification's own limit is 512 characters, and the header is
/// forwarded to the origin — so an unbounded one is a request-smuggling-sized
/// header that Tanod would be amplifying rather than merely receiving.
const MAX_TRACESTATE: usize = 512;

/// A 16-byte trace id. Never all zero: the specification reserves that as
/// "invalid", and a backend that receives it drops the span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceId([u8; 16]);

/// An 8-byte span id, with the same all-zero rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanId([u8; 8]);

impl TraceId {
    /// A fresh id from the OS random source.
    pub fn random() -> TraceId {
        let mut bytes = [0u8; 16];
        fill_random(&mut bytes);
        // All-zero is invalid and a CSPRNG can, with vanishing probability,
        // produce it. Cheaper to handle than to reason about.
        if bytes == [0u8; 16] {
            bytes[15] = 1;
        }
        TraceId(bytes)
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The low 64 bits, for a sampling decision that is stable for a whole
    /// trace. Deriving the decision from the id rather than from a counter is
    /// what keeps every hop of one trace sampled or unsampled together.
    fn low_bits(self) -> u64 {
        let mut v = 0u64;
        for byte in &self.0[8..] {
            v = (v << 8) | u64::from(*byte);
        }
        v
    }
}

impl SpanId {
    pub fn random() -> SpanId {
        let mut bytes = [0u8; 8];
        fill_random(&mut bytes);
        if bytes == [0u8; 8] {
            bytes[7] = 1;
        }
        SpanId(bytes)
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Both indices are masked into 0..16, so neither can be out of range.
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Fill `out` with random bytes, falling back rather than failing.
///
/// `getrandom` can fail only when the OS entropy source is unavailable, which
/// on a running server means something far worse than an unusable trace id.
/// A proxy must not refuse traffic over telemetry, so the fallback mixes a
/// monotonically increasing counter with the clock. That is not
/// cryptographically strong, and it does not need to be: a trace id is not a
/// capability, it authorises nothing, and the only cost of a guessable one is
/// that somebody could pollute a trace they already had to be able to reach.
fn fill_random(out: &mut [u8]) {
    if getrandom::fill(out).is_ok() {
        return;
    }
    mix_fallback(out);
}

/// The fallback mixer, separated so it can be tested rather than merely
/// hoped about — a fallback that only runs when the OS is broken is a
/// fallback nobody ever finds out is wrong.
fn mix_fallback(out: &mut [u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| {
            // Truncating to 64 bits is the intent — the low bits are the ones
            // that move — so it is spelled as a mask rather than an `as`.
            u64::try_from(d.as_nanos() & u128::from(u64::MAX)).unwrap_or(0)
        });
    let mut state = nanos
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    // A zero state is a fixed point of xorshift; seed it away from that.
    if state == 0 {
        state = 0x2545_f491_4f6c_dd1d;
    }
    for byte in out.iter_mut() {
        // xorshift64*, enough to spread a counter across the whole width.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        // The shift leaves eight significant bits, so the conversion is exact.
        *byte = u8::try_from(state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56).unwrap_or(0);
    }
}

/// The trace context for one request, as Tanod concluded it.
#[derive(Debug, Clone)]
pub struct RequestTrace {
    pub trace_id: TraceId,
    /// The span this request's server span belongs under, when an inbound
    /// context was believed.
    pub parent_span_id: Option<SpanId>,
    /// Tanod's own server span for this request.
    pub span_id: SpanId,
    /// A second span id, minted for the origin fetch and sent upstream as the
    /// parent of whatever the origin records. `None` until the request
    /// actually reaches an origin.
    pub origin_span_id: Option<SpanId>,
    pub sampled: bool,
    /// Whether this context continues an inbound one.
    pub continued: bool,
    pub tracestate: Option<String>,
}

impl RequestTrace {
    /// Start a trace for a request.
    ///
    /// `inbound` is the raw `traceparent` value, already gated on trust by the
    /// caller — this function does not decide who to believe, it only decides
    /// what a believed header means.
    pub fn begin(inbound: Option<&str>, tracestate: Option<&str>, sample: &Sample) -> RequestTrace {
        let parsed = inbound.and_then(parse_traceparent);
        let (trace_id, parent_span_id, parent_sampled, continued) = match parsed {
            Some(p) => (p.trace_id, Some(p.span_id), Some(p.sampled), true),
            None => (TraceId::random(), None, None, false),
        };
        RequestTrace {
            sampled: decide(sample, trace_id, parent_sampled),
            trace_id,
            parent_span_id,
            span_id: SpanId::random(),
            origin_span_id: None,
            continued,
            // Carrying vendor state from an untrusted caller would mean
            // forwarding an arbitrary string to the origin under a header
            // name it is expected to parse, so it rides along only with a
            // context that was believed in the first place.
            tracestate: tracestate
                .filter(|_| continued)
                .filter(|s| !s.is_empty() && s.len() <= MAX_TRACESTATE && is_printable_ascii(s))
                .map(str::to_string),
        }
    }

    /// The `traceparent` to send to the origin.
    ///
    /// Its span id is the *origin fetch* span, not this request's server span,
    /// so whatever the origin records nests under the fetch rather than
    /// alongside it.
    pub fn outbound_traceparent(&self) -> String {
        let span = self.origin_span_id.unwrap_or(self.span_id);
        format!(
            "00-{}-{}-{}",
            self.trace_id.to_hex(),
            span.to_hex(),
            if self.sampled { "01" } else { "00" }
        )
    }
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Head sampling.
///
/// `ParentOrRatio` exists because a trace that is sampled at the edge and
/// unsampled here has a hole in it exactly where the origin governor sits —
/// which is the one place someone debugging origin load needs to look.
fn decide(sample: &Sample, trace_id: TraceId, parent_sampled: Option<bool>) -> bool {
    match sample.mode {
        SampleMode::Always => true,
        SampleMode::Never => false,
        SampleMode::Ratio => ratio(sample.one_in, trace_id),
        SampleMode::ParentOrRatio => match parent_sampled {
            Some(decided) => decided,
            None => ratio(sample.one_in, trace_id),
        },
    }
}

/// Sample one trace in `one_in`, decided from the trace id so that every hop
/// of one trace agrees without having to talk to each other.
fn ratio(one_in: usize, trace_id: TraceId) -> bool {
    match u64::try_from(one_in) {
        Ok(0 | 1) => true,
        Ok(n) => trace_id.low_bits().is_multiple_of(n),
        // A `one_in` wider than u64 means "effectively never", and validation
        // rejects values this large long before here.
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parsed {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub sampled: bool,
}

/// Parse a `traceparent` strictly: `00-<32 hex>-<16 hex>-<2 hex>`.
///
/// Strict on purpose. A lenient parser here would let a caller hand Tanod a
/// malformed id that it then forwards to the origin and writes into a tracing
/// backend, and "we accepted it, so it must be fine" is how a bad id spreads
/// through three systems. Anything that does not parse is treated as absent,
/// and the request gets a fresh trace.
///
/// Future versions are *not* rejected outright: the specification requires a
/// higher version to be parsed for its first four fields if it can be, so
/// `01-…` with a valid prefix is accepted. `ff` is reserved and refused.
pub fn parse_traceparent(raw: &str) -> Option<Parsed> {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    // version(2) - trace(32) - span(16) - flags(2) + three dashes
    if bytes.len() < 55 {
        return None;
    }
    if bytes.get(2) != Some(&b'-') || bytes.get(35) != Some(&b'-') || bytes.get(52) != Some(&b'-') {
        return None;
    }
    let version = hex_byte(bytes.get(0..2)?)?;
    if version == 0xff {
        return None;
    }
    // Version 00 is exactly 55 characters; a longer one is malformed rather
    // than forward-compatible.
    if version == 0 && bytes.len() != 55 {
        return None;
    }

    let mut trace = [0u8; 16];
    for (i, slot) in trace.iter_mut().enumerate() {
        let start = 3 + i * 2;
        *slot = hex_byte(bytes.get(start..start + 2)?)?;
    }
    if trace == [0u8; 16] {
        return None;
    }

    let mut span = [0u8; 8];
    for (i, slot) in span.iter_mut().enumerate() {
        let start = 36 + i * 2;
        *slot = hex_byte(bytes.get(start..start + 2)?)?;
    }
    if span == [0u8; 8] {
        return None;
    }

    let flags = hex_byte(bytes.get(53..55)?)?;
    Some(Parsed {
        trace_id: TraceId(trace),
        span_id: SpanId(span),
        sampled: flags & 0x01 == 0x01,
    })
}

fn hex_byte(pair: &[u8]) -> Option<u8> {
    let hi = hex_digit(*pair.first()?)?;
    let lo = hex_digit(*pair.get(1)?)?;
    Some(hi << 4 | lo)
}

/// Lowercase only. The specification says the field is lowercase hex, and
/// accepting uppercase would mean two spellings of one id — which matters
/// because the id is a map key in every backend that receives it.
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Should an inbound trace context from this peer be believed?
pub fn believe_inbound(policy: TrustIncoming, peer_trusted: bool) -> bool {
    match policy {
        TrustIncoming::Always => true,
        TrustIncoming::Never => false,
        TrustIncoming::FromTrustedProxies => peer_trusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn sample(mode: SampleMode, one_in: usize) -> Sample {
        Sample { mode, one_in }
    }

    #[test]
    fn a_valid_traceparent_round_trips() {
        let p = parse_traceparent(VALID).unwrap();
        assert_eq!(p.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(p.span_id.to_hex(), "00f067aa0ba902b7");
        assert!(p.sampled);
    }

    #[test]
    fn the_sampled_flag_is_only_the_low_bit() {
        let unsampled = VALID.replace("-01", "-00");
        assert!(!parse_traceparent(&unsampled).unwrap().sampled);
        // Other flag bits are reserved and must not be read as "sampled".
        let other = VALID.replace("-01", "-02");
        assert!(!parse_traceparent(&other).unwrap().sampled);
        let both = VALID.replace("-01", "-03");
        assert!(parse_traceparent(&both).unwrap().sampled);
    }

    #[test]
    fn all_zero_ids_are_rejected() {
        // The specification reserves both as "invalid"; a backend that gets
        // one drops the span, so accepting it would produce a trace that
        // silently goes nowhere.
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01").is_none()
        );
    }

    #[test]
    fn malformed_headers_are_rejected_rather_than_guessed_at() {
        for bad in [
            "",
            "00",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            // uppercase hex: one id with two spellings
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            // non-hex
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            // wrong separators
            "00_4bf92f3577b34da6a3ce929d0e0e4736_00f067aa0ba902b7_01",
            // reserved version
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // version 00 with trailing junk
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert!(parse_traceparent(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_future_version_is_parsed_for_the_fields_it_shares() {
        // Required by the specification: a higher version keeps the first four
        // fields, so refusing it would break every trace the day the spec moves.
        let p = parse_traceparent(
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-somethingnew",
        )
        .unwrap();
        assert_eq!(p.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn an_absent_context_starts_a_new_trace() {
        let t = RequestTrace::begin(None, None, &sample(SampleMode::Always, 1));
        assert!(!t.continued);
        assert!(t.parent_span_id.is_none());
        assert_ne!(t.trace_id.to_hex(), "0".repeat(32));
    }

    #[test]
    fn a_believed_context_is_continued_not_restarted() {
        let t = RequestTrace::begin(Some(VALID), None, &sample(SampleMode::ParentOrRatio, 1));
        assert!(t.continued);
        assert_eq!(t.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(t.parent_span_id.unwrap().to_hex(), "00f067aa0ba902b7");
        // The server span is Tanod's own, never the caller's.
        assert_ne!(t.span_id.to_hex(), "00f067aa0ba902b7");
    }

    #[test]
    fn a_garbage_context_does_not_lose_the_request() {
        // The failure mode that matters: an unparseable header must cost a
        // continued trace, never the request.
        let t = RequestTrace::begin(
            Some("not-a-traceparent"),
            None,
            &sample(SampleMode::Always, 1),
        );
        assert!(!t.continued);
        assert!(t.sampled);
    }

    #[test]
    fn tracestate_rides_only_with_a_believed_context() {
        let with = RequestTrace::begin(
            Some(VALID),
            Some("vendor=abc"),
            &sample(SampleMode::Always, 1),
        );
        assert_eq!(with.tracestate.as_deref(), Some("vendor=abc"));

        let without = RequestTrace::begin(None, Some("vendor=abc"), &sample(SampleMode::Always, 1));
        assert!(
            without.tracestate.is_none(),
            "vendor state from an ignored context must not be forwarded"
        );
    }

    #[test]
    fn an_oversized_or_unprintable_tracestate_is_dropped() {
        let long = "a".repeat(MAX_TRACESTATE + 1);
        let t = RequestTrace::begin(Some(VALID), Some(&long), &sample(SampleMode::Always, 1));
        assert!(t.tracestate.is_none(), "an unbounded header was forwarded");

        let injected = RequestTrace::begin(
            Some(VALID),
            Some("a\r\nX: b"),
            &sample(SampleMode::Always, 1),
        );
        assert!(injected.tracestate.is_none());
    }

    #[test]
    fn parent_sampling_follows_the_caller_when_it_can() {
        let unsampled = VALID.replace("-01", "-00");
        let t = RequestTrace::begin(
            Some(&unsampled),
            None,
            &sample(SampleMode::ParentOrRatio, 1),
        );
        assert!(!t.sampled, "a caller's decision not to sample was ignored");

        // one_in is large enough that a ratio decision would almost certainly
        // be "no", so a `true` here can only have come from the caller.
        let t = RequestTrace::begin(
            Some(VALID),
            None,
            &sample(SampleMode::ParentOrRatio, 1_000_000),
        );
        assert!(t.sampled, "a caller's decision to sample was ignored");
    }

    #[test]
    fn ratio_mode_overrides_the_caller() {
        // VALID says sampled=01. Its trace id's low bits are 0xa3ce929d0e0e4736,
        // and 0x36 mod 4 is 2, so a one-in-four ratio must decide *not* to
        // sample it — which is only observable if the caller was overridden.
        let t = RequestTrace::begin(Some(VALID), None, &sample(SampleMode::Ratio, 4));
        assert!(!t.sampled, "ratio mode deferred to the caller");
        let t = RequestTrace::begin(Some(VALID), None, &sample(SampleMode::Never, 1));
        assert!(!t.sampled);
    }

    #[test]
    fn one_in_n_samples_roughly_one_in_n() {
        // Not a statistics test — a smoke test that the decision is neither
        // stuck on nor stuck off, which is how a broken ratio usually fails.
        let mut sampled = 0;
        for _ in 0..4000 {
            if RequestTrace::begin(None, None, &sample(SampleMode::Ratio, 10)).sampled {
                sampled += 1;
            }
        }
        assert!(
            (150..=650).contains(&sampled),
            "one-in-10 sampled {sampled} of 4000"
        );
    }

    #[test]
    fn one_in_zero_or_one_samples_everything() {
        for n in [0, 1] {
            let t = RequestTrace::begin(None, None, &sample(SampleMode::Ratio, n));
            assert!(t.sampled, "one_in: {n} dropped a trace");
        }
    }

    #[test]
    fn the_outbound_header_carries_the_origin_span_not_the_server_span() {
        let mut t = RequestTrace::begin(Some(VALID), None, &sample(SampleMode::Always, 1));
        let origin = SpanId::random();
        t.origin_span_id = Some(origin);
        let header = t.outbound_traceparent();
        assert!(header.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
        assert!(header.contains(&origin.to_hex()));
        assert!(header.ends_with("-01"));
        // And it parses back, which is the property the origin depends on.
        assert!(parse_traceparent(&header).is_some());
    }

    #[test]
    fn an_unsampled_request_says_so_downstream() {
        let t = RequestTrace::begin(None, None, &sample(SampleMode::Never, 1));
        assert!(t.outbound_traceparent().ends_with("-00"));
    }

    #[test]
    fn trust_gating_matches_the_forwarded_header_rules() {
        assert!(believe_inbound(TrustIncoming::Always, false));
        assert!(!believe_inbound(TrustIncoming::Never, true));
        assert!(believe_inbound(TrustIncoming::FromTrustedProxies, true));
        assert!(!believe_inbound(TrustIncoming::FromTrustedProxies, false));
    }

    #[test]
    fn generated_ids_are_not_all_the_same() {
        let ids: std::collections::HashSet<String> =
            (0..64).map(|_| TraceId::random().to_hex()).collect();
        assert_eq!(ids.len(), 64, "trace ids repeated");
        let spans: std::collections::HashSet<String> =
            (0..64).map(|_| SpanId::random().to_hex()).collect();
        assert_eq!(spans.len(), 64, "span ids repeated");
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        let t = TraceId([0xab; 16]);
        assert_eq!(t.to_hex(), "ab".repeat(16));
        let s = SpanId([0x0f; 8]);
        assert_eq!(s.to_hex(), "0f".repeat(8));
    }

    #[test]
    fn the_fallback_mixer_produces_distinct_ids() {
        // The path taken when the OS entropy source is unavailable. Starving
        // `getrandom` is not possible from safe code, so the mixer is called
        // directly — otherwise this test would pass without ever reaching it.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let mut bytes = [0u8; 16];
            mix_fallback(&mut bytes);
            assert_ne!(bytes, [0u8; 16], "the fallback produced an invalid id");
            seen.insert(bytes);
        }
        assert_eq!(seen.len(), 256, "the fallback repeated an id");
    }
}
