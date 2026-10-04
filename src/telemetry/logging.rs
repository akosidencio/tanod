//! Structured access logs.
//!
//! # What is deliberately absent
//!
//! No `Authorization`, no `Cookie`, no request or response bodies, and no
//! query string. The query string is the subtle one: it routinely carries
//! session tokens, password-reset codes and signed URLs, and an access log is
//! the single most-copied artifact in an incident. The path alone is logged.
//!
//! # Correlation
//!
//! `trace_id` and `span_id` are on every line, whether or not spans are
//! exported anywhere. They are what joins this line to the origin's own logs
//! for the same request, and — because the id Tanod concluded is the one it
//! forwarded in `traceparent` — the join works even when the origin has no
//! idea Tanod exists.

use super::json::{escape_into, field_str};
use std::fmt::Write as _;

/// One access log line.
pub struct AccessLog<'a> {
    pub method: &'a str,
    /// Path only. Never the query string.
    pub path: &'a str,
    pub route: &'a str,
    pub class: &'a str,
    pub cache: &'a str,
    pub upstream: Option<&'a str>,
    /// The client address Tanod concluded, which for a request from a
    /// trusted proxy is the forwarded one and otherwise the connection peer.
    /// Not the raw header: see [`crate::net::forwarded`].
    pub client: &'a str,
    /// `http` or `https` — the scheme the *client* used, which is also the
    /// scheme in the cache key.
    pub scheme: &'a str,
    pub status: u16,
    pub shed: bool,
    pub origin_ms: u128,
    pub total_ms: u128,
    /// Where the origin permit was returned.
    ///
    /// `origin_end` means the response was spooled, so this is the instant the
    /// origin finished. `body_end` means it was not, so a slow client could
    /// have delayed it. `-` means the request never held a permit. The
    /// distinction is the difference between a real capacity measurement and
    /// one contaminated by client behaviour.
    pub permit_released_at: &'a str,
    /// What the response spool did: `complete`, `body_too_large`,
    /// `budget_exhausted`, or `-` when this request was not spooled.
    pub spool: &'a str,
    /// W3C trace id, 32 lowercase hex characters. Always present: correlation
    /// does not depend on whether anything is exporting spans.
    pub trace_id: &'a str,
    /// Tanod's own server span for this request.
    pub span_id: &'a str,
    /// Whether this request joined a trace the caller had already started, or
    /// began one. Useful for finding the hop where propagation broke.
    pub trace_continued: bool,
    /// The configuration generation that served this request. A reload
    /// mid-incident otherwise makes two lines incomparable with no way to see
    /// it in the log.
    pub generation: u64,
}

impl AccessLog<'_> {
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(224);
        s.push('{');
        write_str(&mut s, "method", self.method);
        write_str(&mut s, "path", self.path);
        write_str(&mut s, "route", self.route);
        write_str(&mut s, "class", self.class);
        write_str(&mut s, "cache", self.cache);
        write_str(&mut s, "upstream", self.upstream.unwrap_or("-"));
        write_str(&mut s, "client", self.client);
        write_str(&mut s, "scheme", self.scheme);
        write_str(&mut s, "permit_released", self.permit_released_at);
        write_str(&mut s, "spool", self.spool);
        write_str(&mut s, "trace_id", self.trace_id);
        write_str(&mut s, "span_id", self.span_id);
        let _ = write!(
            s,
            "\"status\":{},\"shed\":{},\"origin_ms\":{},\"total_ms\":{},\
             \"trace_continued\":{},\"generation\":{}}}",
            self.status,
            self.shed,
            self.origin_ms,
            self.total_ms,
            self.trace_continued,
            self.generation
        );
        s
    }

    pub fn to_text(&self) -> String {
        let mut s = String::with_capacity(224);
        for (key, value) in [
            ("method", self.method),
            ("path", self.path),
            ("route", self.route),
            ("class", self.class),
            ("cache", self.cache),
            ("upstream", self.upstream.unwrap_or("-")),
            ("client", self.client),
            ("scheme", self.scheme),
            ("permit_released", self.permit_released_at),
            ("spool", self.spool),
            ("trace_id", self.trace_id),
            ("span_id", self.span_id),
        ] {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(key);
            s.push_str("=\"");
            escape_into(&mut s, value);
            s.push('"');
        }
        let _ = write!(
            s,
            " status={} shed={} origin_ms={} total_ms={} trace_continued={} generation={}",
            self.status,
            self.shed,
            self.origin_ms,
            self.total_ms,
            self.trace_continued,
            self.generation
        );
        s
    }
}

/// One `"key":"value",` field. Delegates to [`super::json`] so the access log
/// and the OTLP payload cannot drift apart on escaping.
fn write_str(out: &mut String, key: &str, value: &str) {
    field_str(out, key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(path: &str) -> String {
        AccessLog {
            method: "GET",
            path,
            route: "products",
            class: "public_document",
            cache: "hit",
            upstream: Some("next-1:3000"),
            client: "203.0.113.7",
            scheme: "https",
            status: 200,
            shed: false,
            origin_ms: 0,
            total_ms: 3,
            permit_released_at: "origin_end",
            spool: "complete",
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
            span_id: "00f067aa0ba902b7",
            trace_continued: true,
            generation: 3,
        }
        .to_json()
    }

    #[test]
    fn renders_a_plain_line() {
        let out = log("/products/iphone");
        assert!(out.starts_with('{') && out.ends_with('}'));
        assert!(out.contains(r#""path":"/products/iphone""#));
        assert!(out.contains(r#""status":200"#));
    }

    #[test]
    fn a_quote_in_the_path_cannot_break_the_line() {
        let out = log(r#"/a"b"#);
        assert!(out.contains(r#""path":"/a\"b""#), "{out}");
        assert_eq!(
            out.matches(r#"","#).count(),
            12,
            "field count changed: {out}"
        );
    }

    #[test]
    fn a_crafted_path_cannot_forge_fields() {
        // Without escaping this would inject a `status` field of its own.
        let out = log(r#"/x","status":999,"x":"#);
        assert!(out.contains(r#""status":200"#));
        assert!(!out.contains(r#""status":999"#), "{out}");
    }

    #[test]
    fn control_characters_are_escaped() {
        let out = log("/a\nb\tc");
        assert!(out.contains("\\n"), "{out}");
        assert!(out.contains("\\t"), "{out}");
        assert!(
            !out.contains('\n'),
            "a raw newline would split the log line"
        );
    }

    #[test]
    fn null_byte_is_escaped_as_a_unicode_sequence() {
        let out = log("/a\u{0}b");
        assert!(out.contains("\\u0000"), "{out}");
    }

    #[test]
    fn missing_upstream_renders_as_a_dash() {
        let out = AccessLog {
            method: "GET",
            path: "/x",
            route: "-",
            class: "public_document",
            cache: "hit",
            upstream: None,
            client: "-",
            scheme: "http",
            status: 200,
            shed: false,
            origin_ms: 0,
            total_ms: 1,
            permit_released_at: "-",
            spool: "-",
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
            span_id: "00f067aa0ba902b7",
            trace_continued: false,
            generation: 1,
        }
        .to_json();
        assert!(out.contains(r#""upstream":"-""#));
    }

    #[test]
    fn every_line_carries_correlation_ids() {
        // The join key between this log and the origin's own. Absent, the
        // whole point of forwarding `traceparent` is lost.
        let out = log("/products/iphone");
        assert!(
            out.contains(r#""trace_id":"4bf92f3577b34da6a3ce929d0e0e4736""#),
            "{out}"
        );
        assert!(out.contains(r#""span_id":"00f067aa0ba902b7""#), "{out}");
        assert!(out.contains(r#""trace_continued":true"#), "{out}");
        assert!(out.contains(r#""generation":3"#), "{out}");
    }

    #[test]
    fn text_format_escapes_attacker_controlled_values() {
        let mut out = log("/safe");
        assert!(out.starts_with('{'));
        let entry = AccessLog {
            method: "GET",
            path: "/x\nforged=true",
            route: "r",
            class: "public_document",
            cache: "hit",
            upstream: None,
            client: "-",
            scheme: "http",
            status: 200,
            shed: false,
            origin_ms: 1,
            total_ms: 2,
            permit_released_at: "body_end",
            spool: "-",
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
            span_id: "00f067aa0ba902b7",
            trace_continued: false,
            generation: 1,
        };
        out = entry.to_text();
        assert!(!out.contains('\n'));
        assert!(out.contains("\\n"));
    }
}
