//! A slow origin that reports its own peak concurrency.
//!
//! Usage: slow-origin [PORT] [RENDER_MS]
//!
//! Every response carries `X-Origin-Concurrency` (in flight when the request
//! began) and `X-Origin-Peak` (highest seen since start). Those two headers are
//! how the benchmark proves the ceiling held without trusting the proxy's own
//! metrics.
//!
//! Three control endpoints exist so a benchmark never has to infer origin work
//! from the proxy's own logs or from headers that may have been replayed out of
//! the proxy's cache:
//!
//! * `GET /__stats`  — `{"in_flight":n,"peak":n,"total":n}` counted by the
//!   origin itself. This is the ground truth for "how many renders did this
//!   actually cost".
//! * `GET /__reset`  — zero the counters, so one process can measure several
//!   phases without a restart clouding the numbers.
//! * `GET /healthz`  — liveness, deliberately excluded from the counters so an
//!   active health check cannot inflate a render count.
//! * `GET /tagged/<tag>/<n>` — an ordinary render that also declares
//!   `X-Tanod-Cache-Tags: <tag>`, so a benchmark can prove a purge by tag
//!   removes exactly the entries carrying it and nothing else.
//! * `GET /__fail` / `GET /__heal` — make every *render* answer `502` while
//!   leaving `/healthz` answering `200`. That combination is the whole point:
//!   it is what an origin looks like when its health endpoint is a static
//!   route and its renders are the thing that is broken, and it is the case no
//!   active health check can express. Renders are still counted while failing,
//!   so a benchmark can prove traffic stopped arriving rather than inferring
//!   it from the proxy's own metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Stats {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
    /// Upgraded connections currently held open, and how many there have been.
    /// Counted apart from `total` on purpose: a socket is not a render, and a
    /// benchmark asserting "the upgrade consumed no render capacity" needs the
    /// two numbers to be independently readable.
    sockets_open: AtomicUsize,
    sockets_total: AtomicUsize,
    /// Answer every render with `502` while `/healthz` keeps saying `ok`.
    failing: AtomicBool,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);
    let render_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    let stats = Arc::new(Stats {
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        total: AtomicUsize::new(0),
        sockets_open: AtomicUsize::new(0),
        sockets_total: AtomicUsize::new(0),
        failing: AtomicBool::new(false),
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("slow-origin on 127.0.0.1:{port}, {render_ms}ms per render");

    // Identifies *this* process, so that ids minted here cannot collide with
    // ids minted by a second backend or by this one after a restart. The
    // clock is in it because a pid is reused and the port is not unique
    // across a run that restarts a backend on the same one.
    let instance = format!(
        "{port}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    loop {
        let (mut sock, _) = listener.accept().await?;
        let stats = stats.clone();
        let instance = instance.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            // One request per connection keeps the fixture honest and simple.
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            if n == 0 {
                return;
            }
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();

            // Control endpoints are answered before the counters are touched.
            // A health probe or a stats poll is not origin work, and counting
            // it would corrupt the very number the benchmarks assert on.
            if let Some(body) = control_response(&path, &stats) {
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                return;
            }

            // A WebSocket handshake is not a render either. It takes the
            // socket counters and leaves `total` alone, which is what lets a
            // benchmark prove an upgrade cost no render capacity.
            if path.starts_with("/ws") {
                serve_websocket(sock, &head, &stats).await;
                return;
            }

            let now = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            stats.peak.fetch_max(now, Ordering::SeqCst);
            let seq = stats.total.fetch_add(1, Ordering::SeqCst) + 1;

            // /stream/<n> emits n chunks with a render delay between each,
            // the way a server-rendered page streams a shell and then fills
            // in suspended regions. A buffered response cannot exercise
            // whether coalesced waiters receive chunks as they are produced.
            if path.starts_with("/stream") || path.starts_with("/bigstream") {
                let chunks: usize = path
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(4);
                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Transfer-Encoding: chunked\r\n\
                     Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                     X-Origin-Total: {seq}\r\n\
                     Connection: close\r\n\r\n"
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let _ = sock.flush().await;
                for i in 0..chunks {
                    // The shell goes out immediately; everything after it
                    // costs a render delay.
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(render_ms)).await;
                    }
                    // /bigstream emits chunks large enough that a rate-limited
                    // reader actually stalls the write; /stream keeps them tiny
                    // so the coalescing test stays fast.
                    let piece = if path.starts_with("/bigstream") {
                        format!("<div>{}</div>", "x".repeat(256 * 1024))
                    } else {
                        format!("<div>chunk {i}</div>")
                    };
                    let framed = format!("{:x}\r\n{piece}\r\n", piece.len());
                    if sock.write_all(framed.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = sock.flush().await;
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            // Bodies that are deliberately wrong on the wire. Both look like
            // a normal cacheable response right up to the point where they
            // stop, which is what makes them worth testing: a cache that
            // stores what it received rather than what was promised would
            // serve the truncation to everyone afterwards.
            if path.starts_with("/truncated") {
                let promised: usize = tail_number(&path).unwrap_or(4096);
                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Content-Length: {promised}\r\n\
                     Cache-Control: public, max-age=60\r\n\
                     X-Origin-Total: {seq}\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                // Half of what was promised, then the connection closes.
                let _ = sock.write_all(&vec![b'x'; promised / 2]).await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            if path.starts_with("/badchunk") {
                let head = "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Transfer-Encoding: chunked\r\n\
                     Cache-Control: public, max-age=60\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                // One well-formed chunk, then no terminating `0\r\n\r\n`.
                let _ = sock.write_all(b"10\r\n0123456789abcdef\r\n").await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            // /echo-headers reports what the *origin* received, which is the
            // only place the forwarded-header rules can be checked from. A
            // proxy that concluded the right client address and then sent a
            // different one upstream would look correct in its own access log
            // and be wrong everywhere the origin uses the value.
            if path.starts_with("/echo-headers") {
                // `traceparent` and `tracestate` are echoed for the same
                // reason the forwarded headers are: what the origin *received*
                // is the only trustworthy witness to what the proxy sent, and
                // reading the proxy's own log would be asking the component
                // under test to grade itself.
                let body = format!(
                    "{{\"x_forwarded_for\":\"{}\",\"x_forwarded_proto\":\"{}\",\"forwarded\":\"{}\",\"host\":\"{}\",\"traceparent\":\"{}\",\"tracestate\":\"{}\"}}",
                    header_value(&head, "x-forwarded-for").unwrap_or_default(),
                    header_value(&head, "x-forwarded-proto").unwrap_or_default(),
                    header_value(&head, "forwarded").unwrap_or_default(),
                    header_value(&head, "host").unwrap_or_default(),
                    header_value(&head, "traceparent").unwrap_or_default(),
                    header_value(&head, "tracestate").unwrap_or_default(),
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Cache-Control: private, no-store\r\n\
                     X-Origin-Total: {seq}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            // /rendered/<MiB> separates rendering from transmitting.
            //
            // Every other endpoint here counts a request as in flight until
            // its last byte is written, which conflates the two. A real SSR
            // origin does not: React finishes, the event loop is free, and the
            // bytes drain at whatever pace the network manages. That
            // distinction is the entire subject of the response spool, so the
            // benchmark for it needs an origin that can say "I have finished
            // rendering" while a slow client is still reading.
            if path.starts_with("/rendered") {
                let mib = tail_number(&path).unwrap_or(1);
                let body = format!(
                    "<html><body>rendered {path}{}</body></html>",
                    "x".repeat(mib * 1024 * 1024)
                );
                tokio::time::sleep(Duration::from_millis(render_ms)).await;
                // The render is over. Everything after this line is transport,
                // and the counters must not attribute it to the origin.
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);

                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Content-Length: {}\r\n\
                     Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                     X-Origin-Total: {seq}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                return;
            }

            tokio::time::sleep(Duration::from_millis(render_ms)).await;

            // /validated/* carries an ETag and Last-Modified and answers
            // conditional requests itself, so a test can tell a 304 the origin
            // produced from a 304 the cache produced.
            if path.starts_with("/validated") {
                let etag = format!("\"v-{}\"", tail_number(&path).unwrap_or(1));
                let matched = header_value(&head, "if-none-match")
                    .is_some_and(|value| value.split(',').any(|t| t.trim() == etag));
                let response = if matched {
                    format!(
                        "HTTP/1.1 304 Not Modified\r\n\
                         ETag: {etag}\r\n\
                         Cache-Control: public, max-age=60\r\n\
                         X-Origin-Total: {seq}\r\n\
                         Connection: close\r\n\r\n"
                    )
                } else {
                    let body = format!("<html><body>validated {path}</body></html>");
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: text/html\r\n\
                         Content-Length: {}\r\n\
                         ETag: {etag}\r\n\
                         Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\
                         Cache-Control: public, max-age=60\r\n\
                         Accept-Ranges: bytes\r\n\
                         X-Origin-Total: {seq}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            // /private/* sets a session cookie. Nothing that does this may
            // ever be shared between requests, no matter what the route
            // configuration claims.
            // The session id is unique across processes, not just within one.
            //
            // `user-{seq}` alone was a per-process counter, so two backends —
            // or one backend restarted mid-test — mint the same ids and a
            // benchmark counting distinct sessions reads a fixture collision
            // as a shared response. Wrong in the dangerous direction: it can
            // only ever manufacture a failure or, worse, mask a real one by
            // making the count noisy enough to be given slack.
            // `/tagged/<tag>/<n>` declares an invalidation tag, so a
            // benchmark can prove a purge removes exactly the entries carrying
            // it. The tag is taken from the path rather than a fixed constant
            // so one origin can serve several independent tag groups.
            let cache_tags = path
                .strip_prefix("/tagged/")
                .and_then(|rest| rest.split('/').next())
                .filter(|tag| {
                    !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .map(|tag| format!("X-Tanod-Cache-Tags: {tag}\r\n"))
                .unwrap_or_default();
            let set_cookie = if path.starts_with("/private") {
                format!("Set-Cookie: session=user-{instance}-{seq}; Path=/\r\n")
            } else {
                String::new()
            };
            // /big/* returns a realistically sized SSR document so a slow
            // client has something to be slow about.
            // /big/<MiB> returns a body of that size, so a test can pick one
            // large enough to exceed the socket buffers between origin and
            // client and actually block the downstream write.
            let body = if path.starts_with("/big") {
                let mib: usize = tail_number(&path).unwrap_or(1);
                let filler = "x".repeat(mib * 1024 * 1024);
                format!("<html><body>rendered {path}{filler}</body></html>")
            } else {
                format!("<html><body>rendered {path}</body></html>")
            };
            let peak = stats.peak.load(Ordering::SeqCst);
            // Counted before this point, so a benchmark can tell "the proxy
            // stopped sending here" from "the proxy sent and got a 502".
            let status = if stats.failing.load(Ordering::SeqCst) {
                "502 Bad Gateway"
            } else {
                "200 OK"
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: text/html\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                 X-Origin-Concurrency: {now}\r\n\
                 X-Origin-Peak: {peak}\r\n\
                 X-Origin-Total: {}\r\n\
                 {cache_tags}\
                 {set_cookie}\
                 Connection: close\r\n\r\n{body}",
                body.len(),
                stats.total.load(Ordering::SeqCst),
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            stats.in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Answer the non-render endpoints, or `None` if this path is a real render.
fn control_response(path: &str, stats: &Stats) -> Option<String> {
    let body = match path {
        "/healthz" => "ok".to_string(),
        "/__stats" => format!(
            "{{\"in_flight\":{},\"peak\":{},\"total\":{},\"sockets_open\":{},\"sockets_total\":{}}}",
            stats.in_flight.load(Ordering::SeqCst),
            stats.peak.load(Ordering::SeqCst),
            stats.total.load(Ordering::SeqCst),
            stats.sockets_open.load(Ordering::SeqCst),
            stats.sockets_total.load(Ordering::SeqCst),
        ),
        "/__reset" => {
            // in_flight is deliberately not cleared: it is a live count, and
            // zeroing it while requests are in flight would make it drift
            // negative-by-saturation as they complete.
            stats
                .peak
                .store(stats.in_flight.load(Ordering::SeqCst), Ordering::SeqCst);
            stats.total.store(0, Ordering::SeqCst);
            stats.sockets_total.store(0, Ordering::SeqCst);
            "reset".to_string()
        }
        // Deliberately does not touch `/healthz`. An origin whose probe passes
        // and whose renders fail is the exact case passive observation exists
        // for, and a fixture that failed both would be testing the health
        // checker instead.
        "/__fail" => {
            stats.failing.store(true, Ordering::SeqCst);
            "failing".to_string()
        }
        "/__heal" => {
            stats.failing.store(false, Ordering::SeqCst);
            "healed".to_string()
        }
        _ => return None,
    };
    Some(format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    ))
}

// ---------------------------------------------------------------- protocols

/// The trailing `/<number>` of a path, if there is one.
/// The trailing path segment as a number, ignoring any query string.
///
/// The query has to be stripped here. Benchmarks vary the *cache key* with a
/// query while keeping the path — and therefore the response size — fixed, so
/// without this `/big/4?u=1` silently parses as "no number" and serves 1MiB
/// instead of 4. The failure is a wrong-sized body, not an error, which is the
/// worst way for a fixture to be wrong: the test still passes, against a
/// workload it was not running.
fn tail_number(path: &str) -> Option<usize> {
    path.split(['?', '#'])
        .next()?
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
}

/// One request header's value, matched case-insensitively.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
}

/// A real WebSocket server: RFC 6455 handshake, then an echo of every text
/// frame until the client sends a close.
///
/// Faithful rather than approximate on purpose. A fixture that answered `101`
/// with a wrong `Sec-WebSocket-Accept` would still tunnel bytes through the
/// proxy — which is all Tanod handles — and would therefore pass a test that
/// a browser would fail. Computing the accept key is forty lines and removes
/// that whole class of false confidence.
async fn serve_websocket(mut sock: tokio::net::TcpStream, head: &str, stats: &Arc<Stats>) {
    let Some(key) = header_value(head, "sec-websocket-key") else {
        let _ = sock
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        return;
    };
    let accept = websocket_accept(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if sock.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    let _ = sock.flush().await;

    stats.sockets_open.fetch_add(1, Ordering::SeqCst);
    stats.sockets_total.fetch_add(1, Ordering::SeqCst);

    let mut buf = vec![0u8; 4096];
    while let Ok(n) = sock.read(&mut buf).await {
        if n == 0 {
            break;
        }
        // Only what the echo needs: a client-masked frame with a payload
        // short enough to fit the 7-bit length. The bench client sends
        // nothing else.
        let frame = &buf[..n];
        if frame.len() < 2 {
            break;
        }
        let opcode = frame[0] & 0x0f;
        if opcode == 0x8 {
            // Close: echo it back and hang up, so the client sees a clean
            // shutdown rather than a reset it cannot distinguish from a bug.
            let _ = sock.write_all(&[0x88, 0x00]).await;
            break;
        }
        let masked = frame[1] & 0x80 != 0;
        let length = (frame[1] & 0x7f) as usize;
        let mask_at = 2;
        if !masked || frame.len() < mask_at + 4 + length {
            break;
        }
        let mask = &frame[mask_at..mask_at + 4];
        let payload: Vec<u8> = frame[mask_at + 4..mask_at + 4 + length]
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4])
            .collect();
        // Server frames are never masked.
        let mut out = vec![0x81, payload.len() as u8];
        out.extend_from_slice(&payload);
        if sock.write_all(&out).await.is_err() {
            break;
        }
        let _ = sock.flush().await;
    }
    stats.sockets_open.fetch_sub(1, Ordering::SeqCst);
}

/// `base64(sha1(key + RFC 6455 GUID))`.
fn websocket_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    base64(&sha1(format!("{key}{GUID}").as_bytes()))
}

/// SHA-1, per FIPS 180-4. Present because a fixture should not drag a
/// dependency tree into the workspace for one hash, and because the only
/// alternative — a wrong accept key — would make the WebSocket test prove less
/// than it appears to.
fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (message.len() as u64) * 8;

    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - i * 6)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_published_vectors() {
        assert_eq!(
            sha1(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha1(b"")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn the_accept_key_matches_the_rfc_6455_example() {
        // RFC 6455 §1.3 works the example through end to end. If this is
        // wrong, the fixture still tunnels bytes and the WebSocket benchmark
        // still passes — while a real browser refuses the handshake.
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_pads_every_remainder() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
