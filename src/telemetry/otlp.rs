//! Span export over OTLP/HTTP, JSON encoding.
//!
//! # Why this is hand-written
//!
//! The alternative is `opentelemetry-otlp`, which brings either a gRPC stack
//! (`tonic`, `prost`, `tower`, `hyper`) or an HTTP client stack (`reqwest` and
//! a second TLS implementation). Either one is a larger transitive dependency
//! tree than the whole of the rest of this binary, added to a process whose
//! argument for existing is partly that it is small enough to audit and whose
//! `cargo audit` ignore list already documents four advisories it cannot
//! reach on its own.
//!
//! OTLP/HTTP with the JSON encoding is a specified wire format that every
//! collector accepts. It is a `POST` of one JSON document. Writing that is
//! roughly two hundred lines, all of them testable, and it costs no new
//! dependency at all.
//!
//! What is given up is real: no gRPC, no compression, no retry with backoff,
//! no TLS. The first three do not matter for a batch of spans going to a local
//! collector. The fourth is refused rather than faked — see
//! [`parse_endpoint`].
//!
//! # Telemetry must never be load-bearing
//!
//! Every path here can fail, and none of them can fail a request:
//!
//! * The queue is bounded and full means **drop**. A telemetry buffer that
//!   grows under load is a memory-exhaustion bug that fires precisely during
//!   the incident the traces were wanted for.
//! * Recording a span from the request path is a non-blocking `try_send` and
//!   nothing else. No lock, no await, no allocation the request would not have
//!   made anyway.
//! * An export failure is counted and logged at debug. It is never retried
//!   into an unbounded backlog and never propagated.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::json::{escape_into, quoted};
use super::metrics;
use super::trace::{SpanId, TraceId};
use crate::config::schema::Otlp;

/// OTLP `SpanKind`. Only the two Tanod produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// The request as Tanod served it.
    Server,
    /// The fetch Tanod made to the origin, nested under the server span.
    Client,
}

impl SpanKind {
    /// The wire values from `opentelemetry/proto/trace/v1/trace.proto`.
    fn code(self) -> u8 {
        match self {
            SpanKind::Server => 2,
            SpanKind::Client => 3,
        }
    }
}

/// A span attribute. Keys are `&'static str` so the attribute *names* are a
/// closed set chosen in this crate; only values come from the request.
#[derive(Debug, Clone)]
pub struct Attr {
    pub key: &'static str,
    pub value: AttrValue,
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Attr {
    pub fn str(key: &'static str, value: impl Into<String>) -> Attr {
        Attr {
            key,
            value: AttrValue::Str(value.into()),
        }
    }
    pub fn int(key: &'static str, value: i64) -> Attr {
        Attr {
            key,
            value: AttrValue::Int(value),
        }
    }
    pub fn bool(key: &'static str, value: bool) -> Attr {
        Attr {
            key,
            value: AttrValue::Bool(value),
        }
    }
}

/// One finished span, ready to encode.
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    /// Low cardinality by construction: `"<METHOD> <route id>"`, both of which
    /// come from a closed set — the method is validated against a known list
    /// and the route id is written in the config file. Never the path.
    pub name: String,
    pub kind: SpanKind,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    /// Sets OTLP status `ERROR`. A shed request is an error from the caller's
    /// point of view even though Tanod did exactly what it was told to.
    pub error: bool,
    pub attributes: Vec<Attr>,
}

/// Wall-clock nanoseconds since the epoch, saturating at the u64 ceiling
/// (year 2554) rather than wrapping to a timestamp in the past.
pub fn unix_nanos(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The handle the request path holds.
///
/// Cheap to clone and safe to call from anywhere: `record` never blocks, never
/// awaits and never fails in a way the caller has to handle.
#[derive(Clone)]
pub struct SpanSink {
    tx: mpsc::Sender<SpanRecord>,
    dropped: Arc<AtomicU64>,
}

impl SpanSink {
    pub fn record(&self, span: SpanRecord) {
        match self.tx.try_send(span) {
            Ok(()) => {
                metrics::SPANS.with_label_values(&["recorded"]).inc();
            }
            // Full, or the exporter is gone. Both mean the same thing here:
            // this span is lost and the request carries on untouched.
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::SPANS.with_label_values(&["dropped"]).inc();
            }
        }
    }

    /// Spans lost to a full queue since startup. Also a Prometheus counter;
    /// this is for the admin status document.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Where spans are posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
    /// The `Host` header value, with brackets kept for IPv6 literals.
    pub authority: String,
}

/// Parse `http://host[:port][/path]`.
///
/// `https://` is **refused**, not silently downgraded. A hand-written exporter
/// that spoke cleartext to an endpoint someone had written as `https` would be
/// the same class of failure as a config naming a CA that is never read: the
/// operator has every reason to believe a protection is on. Run a collector as
/// a local sidecar and let it terminate TLS on the way out.
pub fn parse_endpoint(raw: &str) -> Result<Endpoint, String> {
    if raw != raw.trim() {
        return Err("the OTLP endpoint has leading or trailing whitespace".to_string());
    }
    if !raw.is_ascii()
        || raw.bytes().any(|b| b <= 0x20 || b == 0x7f)
        || raw.contains('#')
        || raw.contains('\\')
    {
        return Err(
            "the OTLP endpoint contains whitespace, control characters, a fragment, or a backslash"
                .to_string(),
        );
    }
    if let Some(rest) = raw.strip_prefix("https://") {
        let _ = rest;
        return Err(
            "OTLP endpoints must be `http://`. This exporter is deliberately plaintext-only: \
             run an OpenTelemetry Collector alongside Tanod and let it speak TLS onward, \
             rather than have Tanod claim a transport it does not implement"
                .to_string(),
        );
    }
    let rest = raw
        .strip_prefix("http://")
        .ok_or_else(|| format!("`{raw}` is not an http:// URL"))?;
    if rest.is_empty() {
        return Err("the OTLP endpoint has no host".to_string());
    }

    let (authority, path) = match rest.find('/') {
        Some(i) => {
            let (a, p) = rest.split_at(i);
            (a, p)
        }
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("the OTLP endpoint has no host".to_string());
    }
    if authority.contains('@') {
        return Err("the OTLP endpoint must not carry userinfo".to_string());
    }
    if authority.contains('?') {
        return Err("the OTLP endpoint authority contains a query".to_string());
    }

    // IPv6 literals are bracketed and contain the colons that would otherwise
    // be read as a port separator.
    let (host, port) = if let Some(close) = authority.strip_prefix('[') {
        let (inside, after) = close
            .split_once(']')
            .ok_or_else(|| format!("unterminated IPv6 literal in `{authority}`"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => 4318,
            None => return Err(format!("unexpected `{after}` after the IPv6 literal")),
        };
        (inside.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), parse_port(p)?),
            None => (authority.to_string(), 4318),
        }
    };
    if host.is_empty() {
        return Err("the OTLP endpoint has no host".to_string());
    }

    Ok(Endpoint {
        host,
        port,
        // OTLP/HTTP's default path for traces.
        path: if path.is_empty() {
            "/v1/traces".to_string()
        } else {
            path.to_string()
        },
        authority: authority.to_string(),
    })
}

fn parse_port(raw: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .map_err(|_| format!("`{raw}` is not a TCP port"))
        .and_then(|p| {
            if p == 0 {
                Err("port 0 is not a destination".to_string())
            } else {
                Ok(p)
            }
        })
}

/// The batching exporter, run as a Pingora background service so it shares the
/// server's runtime and shutdown signal.
pub struct OtlpExporter {
    endpoint: Endpoint,
    timeout: Duration,
    max_batch: usize,
    interval: Duration,
    resource: Vec<(String, String)>,
    dropped: Arc<AtomicU64>,
    /// Taken once by `start`. A `BackgroundService` is handed `&self`, so the
    /// receiving half has to be moved out from behind a lock exactly once.
    rx: parking_lot::Mutex<Option<mpsc::Receiver<SpanRecord>>>,
}

/// Build an exporter and the handle the request path records through.
///
/// `resource` becomes the OTLP resource attributes — `service.name` and
/// friends — and is fixed at startup, so nothing a client sends can reach it.
pub fn build(
    cfg: &Otlp,
    resource: Vec<(String, String)>,
) -> Result<(SpanSink, OtlpExporter), String> {
    let endpoint = parse_endpoint(&cfg.endpoint)?;
    let (tx, rx) = mpsc::channel(cfg.max_queue.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    let sink = SpanSink {
        tx,
        dropped: dropped.clone(),
    };
    let exporter = OtlpExporter {
        endpoint,
        timeout: cfg.timeout.as_duration(),
        max_batch: cfg.max_batch.max(1),
        interval: cfg.interval.as_duration(),
        resource,
        dropped,
        rx: parking_lot::Mutex::new(Some(rx)),
    };
    Ok((sink, exporter))
}

impl OtlpExporter {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    async fn flush(&self, batch: &mut Vec<SpanRecord>) {
        if batch.is_empty() {
            return;
        }
        let count = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let body = encode(&self.resource, batch);
        match self.post(&body).await {
            Ok(()) => metrics::SPANS
                .with_label_values(&["exported"])
                .inc_by(count),
            Err(why) => {
                metrics::SPANS
                    .with_label_values(&["export_failed"])
                    .inc_by(count);
                // Debug, not warn. A collector that is down would otherwise
                // fill the log of the process whose job is to stay up.
                log::debug!("OTLP export of {count} span(s) failed: {why}");
            }
        }
        batch.clear();
    }

    fn record_shutdown_drop(&self, count: usize) {
        if count == 0 {
            return;
        }
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.dropped.fetch_add(count, Ordering::Relaxed);
        metrics::SPANS.with_label_values(&["dropped"]).inc_by(count);
        log::debug!("OTLP shutdown deadline dropped {count} queued span(s)");
    }

    /// One `POST`, one connection, no keep-alive.
    ///
    /// A pooled connection would save a handshake every `interval` — a cost
    /// measured in microseconds every couple of seconds — in exchange for
    /// owning connection state, half-open detection and a reconnect policy. It
    /// is not worth it for a background exporter.
    async fn post(&self, body: &str) -> Result<(), String> {
        let attempt = async {
            let mut stream = TcpStream::connect((self.endpoint.host.as_str(), self.endpoint.port))
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let head = format!(
                "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nUser-Agent: tanod/{}\r\nConnection: close\r\n\r\n",
                self.endpoint.path,
                self.endpoint.authority,
                body.len(),
                env!("CARGO_PKG_VERSION"),
            );
            stream
                .write_all(head.as_bytes())
                .await
                .map_err(|e| format!("write headers: {e}"))?;
            stream
                .write_all(body.as_bytes())
                .await
                .map_err(|e| format!("write body: {e}"))?;

            // Read only far enough to see the status line. The response body
            // is a partial-success report nobody acts on, and reading it
            // unbounded would let a misbehaving collector feed this process.
            let mut buf = [0u8; 256];
            let mut seen = Vec::with_capacity(64);
            loop {
                let n = stream
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(buf.get(..n).unwrap_or_default());
                if seen.windows(2).any(|w| w == b"\r\n") || seen.len() >= 512 {
                    break;
                }
            }
            let line = String::from_utf8_lossy(&seen);
            let status = line.split_whitespace().nth(1).unwrap_or("");
            if status.starts_with('2') {
                Ok(())
            } else {
                Err(format!("collector answered `{}`", line.trim_end()))
            }
        };
        match tokio::time::timeout(self.timeout, attempt).await {
            Ok(result) => result,
            Err(_) => Err(format!("timed out after {:?}", self.timeout)),
        }
    }
}

#[async_trait]
impl BackgroundService for OtlpExporter {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let Some(mut rx) = self.rx.lock().take() else {
            log::error!("the OTLP exporter was started twice; the second start does nothing");
            return;
        };
        log::info!(
            "exporting spans to http://{}{}",
            self.endpoint.authority,
            self.endpoint.path
        );
        let mut batch: Vec<SpanRecord> = Vec::with_capacity(self.max_batch);
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    // Flush within one total deadline. Applying `timeout` to
                    // every batch independently lets max_queue/max_batch
                    // multiply the shutdown budget; telemetry must never hold
                    // the service lifecycle open that way.
                    rx.close();
                    let drain = async {
                        loop {
                            if batch.is_empty()
                                && rx.recv_many(&mut batch, self.max_batch).await == 0
                            {
                                break;
                            }
                            self.flush(&mut batch).await;
                        }
                    };
                    if tokio::time::timeout(self.timeout, drain).await.is_err() {
                        self.record_shutdown_drop(batch.len().saturating_add(rx.len()));
                    }
                    return;
                }
                received = rx.recv_many(&mut batch, self.max_batch) => {
                    if received == 0 {
                        // Every sender is gone, which only happens at teardown.
                        self.flush(&mut batch).await;
                        return;
                    }
                    if batch.len() >= self.max_batch {
                        self.flush(&mut batch).await;
                    }
                }
                _ = ticker.tick() => self.flush(&mut batch).await,
            }
        }
    }
}

/// Encode a batch as an OTLP `ExportTraceServiceRequest` in protobuf-JSON.
///
/// The field names are the protobuf JSON mapping's lowerCamelCase forms and
/// the 64-bit timestamps are strings, both of which the mapping requires;
/// getting either wrong produces a document a collector rejects wholesale.
pub fn encode(resource: &[(String, String)], spans: &[SpanRecord]) -> String {
    let mut s = String::with_capacity(256 + spans.len() * 320);
    s.push_str("{\"resourceSpans\":[{\"resource\":{\"attributes\":[");
    for (i, (key, value)) in resource.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"key\":");
        quoted(&mut s, key);
        s.push_str(",\"value\":{\"stringValue\":");
        quoted(&mut s, value);
        s.push_str("}}");
    }
    s.push_str("]},\"scopeSpans\":[{\"scope\":{\"name\":\"tanod\",\"version\":\"");
    escape_into(&mut s, env!("CARGO_PKG_VERSION"));
    s.push_str("\"},\"spans\":[");
    for (i, span) in spans.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        encode_span(&mut s, span);
    }
    s.push_str("]}]}]}");
    s
}

fn encode_span(s: &mut String, span: &SpanRecord) {
    s.push_str("{\"traceId\":\"");
    s.push_str(&span.trace_id.to_hex());
    s.push_str("\",\"spanId\":\"");
    s.push_str(&span.span_id.to_hex());
    s.push('"');
    if let Some(parent) = span.parent_span_id {
        s.push_str(",\"parentSpanId\":\"");
        s.push_str(&parent.to_hex());
        s.push('"');
    }
    s.push_str(",\"name\":");
    quoted(s, &span.name);
    s.push_str(",\"kind\":");
    s.push_str(&span.kind.code().to_string());
    s.push_str(",\"startTimeUnixNano\":\"");
    s.push_str(&span.start_unix_nano.to_string());
    s.push_str("\",\"endTimeUnixNano\":\"");
    s.push_str(&span.end_unix_nano.to_string());
    s.push_str("\",\"attributes\":[");
    for (i, attr) in span.attributes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"key\":");
        quoted(s, attr.key);
        s.push_str(",\"value\":{");
        match &attr.value {
            AttrValue::Str(v) => {
                s.push_str("\"stringValue\":");
                quoted(s, v);
            }
            // Protobuf JSON renders 64-bit integers as strings.
            AttrValue::Int(v) => {
                s.push_str("\"intValue\":\"");
                s.push_str(&v.to_string());
                s.push('"');
            }
            AttrValue::Bool(v) => {
                s.push_str("\"boolValue\":");
                s.push_str(if *v { "true" } else { "false" });
            }
        }
        s.push_str("}}");
    }
    // 0 UNSET, 1 OK, 2 ERROR. Unset rather than OK for a healthy span: OTLP
    // reserves `OK` for a status the application asserted, and asserting it on
    // every request would override a judgement the origin may have made.
    s.push_str("],\"status\":{\"code\":");
    s.push_str(if span.error { "2" } else { "0" });
    s.push_str("}}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::units::Dur;

    fn span() -> SpanRecord {
        SpanRecord {
            trace_id: TraceId::random(),
            span_id: SpanId::random(),
            parent_span_id: None,
            name: "GET products".to_string(),
            kind: SpanKind::Server,
            start_unix_nano: 1_700_000_000_000_000_000,
            end_unix_nano: 1_700_000_000_050_000_000,
            error: false,
            attributes: vec![
                Attr::str("http.request.method", "GET"),
                Attr::int("http.response.status_code", 200),
                Attr::bool("tanod.shed", false),
            ],
        }
    }

    fn otlp(endpoint: &str) -> Otlp {
        Otlp {
            endpoint: endpoint.to_string(),
            timeout: Dur(Duration::from_secs(5)),
            max_queue: 16,
            max_batch: 8,
            interval: Dur(Duration::from_secs(1)),
        }
    }

    #[test]
    fn an_endpoint_parses_into_its_parts() {
        let e = parse_endpoint("http://127.0.0.1:4318/v1/traces").unwrap();
        assert_eq!(e.host, "127.0.0.1");
        assert_eq!(e.port, 4318);
        assert_eq!(e.path, "/v1/traces");
        assert_eq!(e.authority, "127.0.0.1:4318");
    }

    #[test]
    fn the_default_port_and_path_are_otlp_http_defaults() {
        let e = parse_endpoint("http://collector").unwrap();
        assert_eq!(e.port, 4318);
        assert_eq!(e.path, "/v1/traces");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_in_the_host_header() {
        let e = parse_endpoint("http://[::1]:4318/v1/traces").unwrap();
        assert_eq!(e.host, "::1", "the connect address must be unbracketed");
        assert_eq!(e.port, 4318);
        assert_eq!(
            e.authority, "[::1]:4318",
            "the Host header must keep the brackets"
        );

        let bare = parse_endpoint("http://[2001:db8::1]").unwrap();
        assert_eq!(bare.host, "2001:db8::1");
        assert_eq!(bare.port, 4318);
    }

    #[test]
    fn https_is_refused_rather_than_downgraded() {
        // The failure this prevents: a config that says https, an exporter
        // that speaks cleartext, and no way to tell from the outside.
        let err = parse_endpoint("https://collector:4318/v1/traces").unwrap_err();
        assert!(err.contains("http://"), "{err}");
        assert!(err.contains("Collector"), "{err}");
    }

    #[test]
    fn malformed_endpoints_are_refused() {
        for bad in [
            "",
            "collector:4318",
            "http://",
            "http://host:0/v1/traces",
            "http://host:99999/v1/traces",
            "http://host:notaport",
            "http://user@host:4318",
            "http://[::1:4318",
            " http://collector:4318/v1/traces",
            "http://collector:4318/v1/traces ",
            "http://collector:4318/v1/traces#fragment",
            "http://collector:4318/v1/traces HTTP/1.1\r\nX-Injected: yes",
            "http://collector:4318\\v1\\traces",
            "http://collector?tenant=x/v1/traces",
        ] {
            assert!(parse_endpoint(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_encoded_document_has_the_otlp_shape() {
        let body = encode(
            &[("service.name".to_string(), "tanod".to_string())],
            &[span()],
        );
        assert!(body.starts_with("{\"resourceSpans\":[{"));
        assert!(body.contains("\"scopeSpans\""));
        assert!(body.contains("\"service.name\""));
        // Protobuf JSON renders 64-bit fields as strings; a number here is
        // rejected by the collector for the whole batch.
        assert!(
            body.contains("\"startTimeUnixNano\":\"1700000000000000000\""),
            "{body}"
        );
        assert!(
            body.contains("\"intValue\":\"200\""),
            "status code was not string-encoded: {body}"
        );
        assert!(body.contains("\"kind\":2"));
        assert!(body.contains("\"boolValue\":false"));
        assert!(body.ends_with("]}]}]}"));
    }

    #[test]
    fn a_parent_span_id_is_only_emitted_when_there_is_one() {
        let mut s = span();
        assert!(!encode(&[], &[s.clone()]).contains("parentSpanId"));
        s.parent_span_id = Some(SpanId::random());
        assert!(encode(&[], &[s]).contains("parentSpanId"));
    }

    #[test]
    fn an_error_span_carries_otlp_status_error() {
        let mut s = span();
        s.error = true;
        assert!(encode(&[], &[s]).contains("\"status\":{\"code\":2}"));
        assert!(encode(&[], &[span()]).contains("\"status\":{\"code\":0}"));
    }

    #[test]
    fn attacker_controlled_attribute_values_cannot_break_the_document() {
        let mut s = span();
        s.attributes = vec![Attr::str("url.path", "/a\"b\n{\"injected\":true}")];
        let body = encode(&[], &[s]);
        assert!(!body.contains('\n'));
        assert!(!body.contains("\"injected\":true"), "{body}");
        assert!(body.contains(r#"\"injected\""#), "{body}");
    }

    #[test]
    fn a_batch_encodes_as_a_span_array() {
        let body = encode(&[], &[span(), span(), span()]);
        assert_eq!(body.matches("\"traceId\"").count(), 3);
        // One resource and one scope for the whole batch, not one each.
        assert_eq!(body.matches("\"scopeSpans\"").count(), 1);
    }

    #[tokio::test]
    async fn a_full_queue_drops_spans_instead_of_blocking() {
        // The property that matters: recording must never make a request wait.
        let (sink, _exporter) = build(
            &Otlp {
                max_queue: 2,
                ..otlp("http://127.0.0.1:4318/v1/traces")
            },
            vec![],
        )
        .unwrap();
        for _ in 0..50 {
            sink.record(span());
        }
        assert!(sink.dropped() >= 48, "dropped {}", sink.dropped());
    }

    #[tokio::test]
    async fn recording_after_the_exporter_is_gone_is_harmless() {
        let (sink, exporter) = build(&otlp("http://127.0.0.1:4318/v1/traces"), vec![]).unwrap();
        drop(exporter);
        sink.record(span());
        assert_eq!(sink.dropped(), 1);
    }

    #[tokio::test]
    async fn shutdown_flush_has_one_total_deadline_and_counts_the_remainder() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept connections but never answer them, so every attempted batch
        // would consume a full per-request timeout without the outer deadline.
        let collector = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        // Deep enough that the queue cannot empty before the shutdown branch
        // is taken. `tokio::select!` picks at random among ready branches, and
        // on the first poll all three are — the watch, the buffered spans, and
        // `interval`'s immediate first tick. Every ordinary `recv` that wins
        // the draw exports one span through the normal path, so a queue of
        // four could be gone by the time shutdown is handled, leaving nothing
        // to abandon and failing this test on a race rather than a defect.
        const QUEUED: usize = 64;
        let (sink, exporter) = build(
            &Otlp {
                timeout: Dur(Duration::from_millis(50)),
                max_queue: QUEUED,
                max_batch: 1,
                interval: Dur(Duration::from_secs(60)),
                ..otlp(&format!("http://127.0.0.1:{port}/v1/traces"))
            },
            vec![],
        )
        .unwrap();
        for _ in 0..QUEUED {
            sink.record(span());
        }
        // The queue is exactly full, never over: a span dropped here would be
        // a full-channel drop, and would satisfy the assertion below without
        // the shutdown path ever having abandoned anything.
        assert_eq!(sink.dropped(), 0, "a span was dropped before shutdown");

        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { exporter.start(rx).await });
        tx.send(true).unwrap();
        let started = std::time::Instant::now();
        task.await.unwrap();
        collector.abort();

        // One deadline for the whole drain. Applied per batch instead, this
        // collector answers nothing, so 64 batches would cost 64 × 50ms.
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            sink.dropped() > 0,
            "the abandoned queue was not accounted for"
        );
    }

    #[tokio::test]
    async fn an_unreachable_collector_does_not_stall_the_exporter() {
        // Port 1 on loopback refuses. The export must resolve as a failure
        // rather than hang, or a background service holds a shutdown open.
        let (_sink, exporter) = build(
            &Otlp {
                timeout: Dur(Duration::from_millis(500)),
                ..otlp("http://127.0.0.1:1/v1/traces")
            },
            vec![],
        )
        .unwrap();
        let started = std::time::Instant::now();
        assert!(exporter.post("{}").await.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_batch_reaches_a_listening_collector() {
        // End to end over a real socket, against a server that asserts the
        // request line and content type rather than merely accepting bytes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut seen = Vec::new();
            // Read until the body is in hand; the exporter sends a
            // Content-Length and then closes.
            while let Ok(n) = sock.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.ends_with(b"]}]}]}") {
                    break;
                }
            }
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            String::from_utf8_lossy(&seen).to_string()
        });

        let (_sink, exporter) = build(
            &otlp(&format!("http://127.0.0.1:{port}/v1/traces")),
            vec![("service.name".to_string(), "tanod".to_string())],
        )
        .unwrap();
        let body = encode(
            &[("service.name".to_string(), "tanod".to_string())],
            &[span()],
        );
        exporter.post(&body).await.unwrap();

        let request = received.await.unwrap();
        assert!(
            request.starts_with("POST /v1/traces HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("Content-Type: application/json"));
        assert!(request.contains(&format!("Content-Length: {}", body.len())));
        assert!(request.contains("\"resourceSpans\""));
    }

    #[tokio::test]
    async fn a_non_2xx_from_the_collector_is_an_error_not_a_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let (_sink, exporter) =
            build(&otlp(&format!("http://127.0.0.1:{port}/v1/traces")), vec![]).unwrap();
        let err = exporter.post("{}").await.unwrap_err();
        assert!(err.contains("400"), "{err}");
    }
}
