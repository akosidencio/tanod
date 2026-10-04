//! One `POST` of a JSON document, over `http://` or — in a binary built with
//! the `tls` feature — `https://`. Shared by the span and metric exporters.
//!
//! Hand-written for the reason given in [`super::otlp`]: an HTTP client crate
//! would be a larger dependency tree than the rest of this binary. TLS reuses
//! the rustls stack Pingora already links for `server.tls`, so `https://`
//! costs no new dependency either. It exists so Tanod can send straight to a
//! hosted OTLP endpoint (Grafana Cloud and the like) without a collector
//! container beside it whose only job is to add TLS.
//!
//! Server certificates are verified against the platform's CA store
//! (`SSL_CERT_FILE` and `SSL_CERT_DIR` are honoured). There is no switch to
//! turn verification off.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where a document is posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
    /// The `Host` header value, with brackets kept for IPv6 literals.
    pub authority: String,
    pub tls: bool,
}

impl Endpoint {
    /// The endpoint as a URL, for log lines.
    pub fn url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}{}", self.authority, self.path)
    }
}

/// Parse `http[s]://host[:port][/path]`, using `default_path` when the URL has
/// none.
///
/// `https://` in a binary without the `tls` feature is **refused**, not
/// downgraded: an exporter that spoke cleartext to an endpoint written as
/// `https` would leave the operator believing a protection is on.
pub fn parse_endpoint(raw: &str, default_path: &str) -> Result<Endpoint, String> {
    if raw != raw.trim() {
        return Err("the endpoint has leading or trailing whitespace".to_string());
    }
    if !raw.is_ascii()
        || raw.bytes().any(|b| b <= 0x20 || b == 0x7f)
        || raw.contains('#')
        || raw.contains('\\')
    {
        return Err(
            "the endpoint contains whitespace, control characters, a fragment, or a backslash"
                .to_string(),
        );
    }
    let (rest, tls, default_port) = if let Some(rest) = raw.strip_prefix("https://") {
        if !cfg!(feature = "tls") {
            return Err(
                "`https://` needs a binary built with the `tls` feature (the release binary and \
                 the published image have it); this one would have to send in cleartext, so it \
                 refuses instead"
                    .to_string(),
            );
        }
        (rest, true, 443)
    } else if let Some(rest) = raw.strip_prefix("http://") {
        (rest, false, 4318)
    } else {
        return Err(format!("`{raw}` is not an http:// or https:// URL"));
    };
    if rest.is_empty() {
        return Err("the endpoint has no host".to_string());
    }

    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("the endpoint has no host".to_string());
    }
    if authority.contains('@') {
        return Err(
            "the endpoint must not carry userinfo; put credentials in `headers`".to_string(),
        );
    }
    if authority.contains('?') {
        return Err("the endpoint authority contains a query".to_string());
    }

    // IPv6 literals are bracketed and contain the colons that would otherwise
    // be read as a port separator.
    let (host, port) = if let Some(close) = authority.strip_prefix('[') {
        let (inside, after) = close
            .split_once(']')
            .ok_or_else(|| format!("unterminated IPv6 literal in `{authority}`"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => default_port,
            None => return Err(format!("unexpected `{after}` after the IPv6 literal")),
        };
        (inside.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), parse_port(p)?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err("the endpoint has no host".to_string());
    }

    Ok(Endpoint {
        host,
        port,
        path: if path.is_empty() {
            default_path.to_string()
        } else {
            path.to_string()
        },
        authority: authority.to_string(),
        tls,
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

/// Headers this module writes itself. Letting config set them would let a
/// typo desynchronise the framing of the request.
const RESERVED: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
    "user-agent",
];

/// Check configured headers and render them as request lines.
///
/// Values usually carry credentials (`Authorization: Basic …`), so they are
/// never echoed in an error message.
pub fn render_headers(headers: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    for (name, value) in headers {
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !token {
            return Err(format!("header name {name:?} is not a valid HTTP token"));
        }
        if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "header {name} is set by Tanod and cannot be configured"
            ));
        }
        if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(format!(
                "the value of header {name} contains a control character"
            ));
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    Ok(out)
}

/// A configured destination: endpoint, extra headers and a deadline.
pub struct Transport {
    endpoint: Endpoint,
    headers: String,
    timeout: Duration,
    #[cfg(feature = "tls")]
    tls: Option<pingora_core::tls::TlsConnector>,
}

impl Transport {
    pub fn new(
        endpoint: Endpoint,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Transport, String> {
        let headers = render_headers(headers)?;
        #[cfg(feature = "tls")]
        let tls = if endpoint.tls {
            Some(tls_connector()?)
        } else {
            None
        };
        Ok(Transport {
            endpoint,
            headers,
            timeout,
            #[cfg(feature = "tls")]
            tls,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// One `POST`, one connection, no keep-alive.
    ///
    /// A pooled connection would save a handshake per export — every few
    /// seconds for spans, every minute for metrics — in exchange for owning
    /// connection state, half-open detection and a reconnect policy.
    pub async fn post_json(&self, body: &str) -> Result<(), String> {
        let attempt = async {
            let stream = TcpStream::connect((self.endpoint.host.as_str(), self.endpoint.port))
                .await
                .map_err(|e| format!("connect: {e}"))?;
            #[cfg(feature = "tls")]
            if let Some(connector) = &self.tls {
                let name = pingora_core::tls::ServerName::try_from(self.endpoint.host.clone())
                    .map_err(|e| format!("server name {}: {e}", self.endpoint.host))?;
                let stream = connector
                    .connect(name, stream)
                    .await
                    .map_err(|e| format!("TLS handshake: {e}"))?;
                return self.exchange(stream, body).await;
            }
            self.exchange(stream, body).await
        };
        match tokio::time::timeout(self.timeout, attempt).await {
            Ok(result) => result,
            Err(_) => Err(format!("timed out after {:?}", self.timeout)),
        }
    }

    async fn exchange<S>(&self, mut stream: S, body: &str) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nUser-Agent: tanod/{}\r\nConnection: close\r\n{}\r\n",
            self.endpoint.path,
            self.endpoint.authority,
            body.len(),
            env!("CARGO_PKG_VERSION"),
            self.headers,
        );
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| format!("write headers: {e}"))?;
        stream
            .write_all(body.as_bytes())
            .await
            .map_err(|e| format!("write body: {e}"))?;
        stream.flush().await.map_err(|e| format!("flush: {e}"))?;

        // Read only far enough to see the status line. The response body is a
        // partial-success report nobody acts on, and reading it unbounded
        // would let a misbehaving collector feed this process.
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
        let line = line.lines().next().unwrap_or("");
        let status = line.split_whitespace().nth(1).unwrap_or("");
        if status.starts_with('2') {
            Ok(())
        } else {
            Err(format!("collector answered `{}`", line.trim_end()))
        }
    }
}

/// A connector verifying against the platform CA store. Built once per
/// exporter, at startup, so a host with no CA bundle fails at boot rather than
/// on the first export a minute later.
#[cfg(feature = "tls")]
fn tls_connector() -> Result<pingora_core::tls::TlsConnector, String> {
    use pingora_core::tls::{ClientConfig, RootCertStore, TlsConnector};
    pingora_core::tls::install_default_crypto_provider();
    let mut roots = RootCertStore::empty();
    pingora_core::tls::load_platform_certs_incl_env_into_store(&mut roots)
        .map_err(|e| format!("loading the platform CA certificates: {e}"))?;
    if roots.is_empty() {
        return Err(
            "no CA certificates were found to verify the endpoint with; install the \
             ca-certificates package or point SSL_CERT_FILE at a bundle"
                .to_string(),
        );
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(std::sync::Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_parses_into_its_parts() {
        let e = parse_endpoint("http://collector:4318/v1/metrics", "/x").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str(), e.tls),
            ("collector", 4318, "/v1/metrics", false)
        );
        let e = parse_endpoint("http://collector", "/v1/metrics").unwrap();
        assert_eq!(e.path, "/v1/metrics");
    }

    #[test]
    fn https_depends_on_the_tls_feature() {
        let result = parse_endpoint("https://otlp.example.com/otlp/v1/metrics", "/v1/metrics");
        if cfg!(feature = "tls") {
            let e = result.unwrap();
            assert_eq!((e.port, e.tls), (443, true));
            assert_eq!(e.url(), "https://otlp.example.com/otlp/v1/metrics");
        } else {
            assert!(result.unwrap_err().contains("tls"));
        }
    }

    #[test]
    fn credentials_belong_in_headers_not_the_url() {
        assert!(
            parse_endpoint("http://user:pw@host/v1", "/")
                .unwrap_err()
                .contains("headers")
        );
    }

    #[test]
    fn headers_are_rendered_and_checked() {
        let mut h = BTreeMap::new();
        h.insert("Authorization".to_string(), "Basic abc==".to_string());
        assert_eq!(
            render_headers(&h).unwrap(),
            "Authorization: Basic abc==\r\n"
        );

        let mut bad = BTreeMap::new();
        bad.insert("X-Ok".to_string(), "a\r\nInjected: 1".to_string());
        let err = render_headers(&bad).unwrap_err();
        assert!(err.contains("control character"));
        assert!(
            !err.contains("Injected"),
            "the value must not be echoed: {err}"
        );

        let mut reserved = BTreeMap::new();
        reserved.insert("Content-Length".to_string(), "1".to_string());
        assert!(
            render_headers(&reserved)
                .unwrap_err()
                .contains("set by Tanod")
        );

        let mut name = BTreeMap::new();
        name.insert("Bad Name".to_string(), "v".to_string());
        assert!(render_headers(&name).is_err());
    }
}
