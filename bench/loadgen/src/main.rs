//! A closed-loop HTTP/1.1 load generator that reports what a performance
//! comparison needs and nothing it cannot measure.
//!
//! Two modes:
//!
//! * `run` keeps `--connections` keep-alive connections busy for `--duration`
//!   seconds after `--warmup` seconds that are executed but not recorded.
//!   Each connection sends its next request as soon as the previous response
//!   has been read completely, so throughput is what the server sustains at
//!   that concurrency, and latency is service time plus queueing inside the
//!   server — never queueing inside this tool.
//! * `burst` opens `--connections` connections first, then releases one
//!   request on every one of them at the same instant. That is the shape of
//!   a cold-cache herd, and the only way to ask "how many renders did N
//!   simultaneous identical requests cost" without the arrival spread of
//!   `xargs curl` deciding the answer.
//!
//! Targets come from repeated `--path` (equal weights) or a `--mix` file with
//! one target per line: `weight|path|Header: value|...`. In a path, `{n:K}`
//! becomes a uniformly random integer below K and `{u}` a value unique to this
//! process, so a mix can describe a hot key space and a never-cached one.
//!
//! `--watch-pid` (repeatable) samples a process's CPU time over the measured
//! window and its resident set, so CPU and memory come from the kernel's own
//! accounting rather than from anything the server says about itself.
//!
//! Output is one JSON object on stdout; progress and errors go to stderr.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Target {
    weight: u32,
    path: String,
    headers: String,
}

struct Options {
    mode: String,
    addr: String,
    host: String,
    targets: Vec<Target>,
    connections: usize,
    threads: usize,
    duration: Duration,
    warmup: Duration,
    timeout: Duration,
    watch: Vec<u32>,
    label: String,
    slow_readers: usize,
    slow_bps: u64,
}

fn usage() -> ! {
    eprintln!(
        "usage: loadgen run|burst --addr HOST:PORT [--host H] (--path P ... | --mix FILE)\n\
         \x20      [--connections N] [--threads T] [--duration S] [--warmup S]\n\
         \x20      [--timeout S] [--watch-pid PID ...] [--label L]\n\
         \x20      [--slow-readers K --slow-bps B]   (burst only)"
    );
    std::process::exit(2);
}

fn parse_options() -> Options {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| usage());
    if mode != "run" && mode != "burst" {
        usage();
    }
    let mut opts = Options {
        mode,
        addr: String::new(),
        host: String::new(),
        targets: Vec::new(),
        connections: 16,
        threads: 2,
        duration: Duration::from_secs(10),
        warmup: Duration::from_secs(2),
        timeout: Duration::from_secs(30),
        watch: Vec::new(),
        label: String::new(),
        slow_readers: 0,
        slow_bps: 0,
    };
    let seconds = |v: String| Duration::from_secs_f64(v.parse::<f64>().unwrap_or_else(|_| usage()));
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--addr" => opts.addr = value,
            "--host" => opts.host = value,
            "--path" => opts.targets.push(Target {
                weight: 1,
                path: value,
                headers: String::new(),
            }),
            "--mix" => opts.targets.extend(read_mix(&value)),
            "--connections" => opts.connections = value.parse().unwrap_or_else(|_| usage()),
            "--threads" => opts.threads = value.parse().unwrap_or_else(|_| usage()),
            "--duration" => opts.duration = seconds(value),
            "--warmup" => opts.warmup = seconds(value),
            "--timeout" => opts.timeout = seconds(value),
            "--watch-pid" => opts.watch.push(value.parse().unwrap_or_else(|_| usage())),
            "--label" => opts.label = value,
            "--slow-readers" => opts.slow_readers = value.parse().unwrap_or_else(|_| usage()),
            "--slow-bps" => opts.slow_bps = value.parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    if opts.addr.is_empty() || opts.targets.is_empty() || opts.connections == 0 {
        usage();
    }
    if opts.host.is_empty() {
        opts.host = opts.addr.clone();
    }
    opts
}

fn read_mix(file: &str) -> Vec<Target> {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| {
        eprintln!("loadgen: cannot read mix {file}: {e}");
        std::process::exit(2);
    });
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('|').map(str::trim);
            let weight = fields.next().and_then(|w| w.parse().ok()).unwrap_or(0);
            let path = fields.next().unwrap_or("/").to_string();
            let headers = fields
                .filter(|h| !h.is_empty())
                .map(|h| format!("{h}\r\n"))
                .collect();
            Target {
                weight,
                path,
                headers,
            }
        })
        .filter(|t| t.weight > 0)
        .collect()
}

/// xorshift64*: per-connection, so no shared state on the request path.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn render_path(template: &str, rng: &mut Rng) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let token = &rest[start + 1..start + len];
        if token == "u" {
            let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
            out.push_str(&format!("{}-{n}", std::process::id()));
        } else if let Some(k) = token.strip_prefix("n:") {
            out.push_str(&rng.below(k.parse().unwrap_or(1)).to_string());
        } else {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

#[derive(Default)]
struct Sample {
    target: usize,
    /// Read at `--slow-bps` (burst mode), so its latency is the reader's own.
    slow: bool,
    total_us: u32,
    ttfb_us: u32,
    status: u16,
}

#[derive(Default)]
struct Tally {
    samples: Vec<Sample>,
    errors: u64,
    error_kinds: Vec<String>,
    bytes: u64,
}

struct Response {
    status: u16,
    ttfb: Duration,
    body_bytes: u64,
    close: bool,
}

/// One HTTP/1.1 exchange on an open connection.
async fn exchange(
    sock: &mut TcpStream,
    request: &[u8],
    buf: &mut Vec<u8>,
    slow_bps: u64,
) -> std::io::Result<Response> {
    let started = Instant::now();
    sock.write_all(request).await?;
    buf.clear();
    let mut ttfb = None;
    // Header.
    let header_end = loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = read_more(sock, buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "closed before response header",
            ));
        }
        ttfb.get_or_insert_with(|| started.elapsed());
    };
    let ttfb = ttfb.unwrap_or_else(|| started.elapsed());
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let header = |name: &str| {
        head.lines()
            .skip(1)
            .find_map(|line| line.strip_prefix(name).map(|v| v.trim().to_string()))
    };
    let close = header("connection:").is_some_and(|v| v.contains("close"));
    let chunked = header("transfer-encoding:").is_some_and(|v| v.contains("chunked"));
    let length: Option<u64> = header("content-length:").and_then(|v| v.parse().ok());
    buf.drain(..header_end);

    let mut body_bytes = 0u64;
    let pace = |got: u64| async move {
        if slow_bps > 0 && got > 0 {
            tokio::time::sleep(Duration::from_secs_f64(got as f64 / slow_bps as f64)).await;
        }
    };
    if chunked {
        loop {
            let line_end = loop {
                if let Some(pos) = find(buf, b"\r\n") {
                    break pos;
                }
                if read_more(sock, buf).await? == 0 {
                    return Err(eof("chunk size"));
                }
            };
            let size_text = String::from_utf8_lossy(&buf[..line_end]).to_string();
            let size = u64::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| std::io::Error::other(format!("bad chunk size {size_text:?}")))?;
            buf.drain(..line_end + 2);
            if size == 0 {
                // Trailers, then the empty line.
                loop {
                    if let Some(pos) = find(buf, b"\r\n") {
                        let empty = pos == 0;
                        buf.drain(..pos + 2);
                        if empty {
                            break;
                        }
                        continue;
                    }
                    if read_more(sock, buf).await? == 0 {
                        return Err(eof("trailer"));
                    }
                }
                break;
            }
            let mut need = size + 2;
            while need > 0 {
                if buf.is_empty() && read_more(sock, buf).await? == 0 {
                    return Err(eof("chunk body"));
                }
                let take = need.min(buf.len() as u64) as usize;
                buf.drain(..take);
                need -= take as u64;
                pace(take as u64).await;
            }
            body_bytes += size;
        }
    } else if let Some(length) = length {
        let mut need = length;
        let have = (buf.len() as u64).min(need);
        need -= have;
        buf.drain(..have as usize);
        pace(have).await;
        let mut scratch = [0u8; 64 * 1024];
        while need > 0 {
            let want = scratch.len().min(need as usize);
            let want = if slow_bps > 0 { want.min(4096) } else { want };
            let n = sock.read(&mut scratch[..want]).await?;
            if n == 0 {
                return Err(eof("body"));
            }
            need -= n as u64;
            pace(n as u64).await;
        }
        body_bytes = length;
    } else {
        // Delimited by close.
        let mut scratch = [0u8; 64 * 1024];
        body_bytes = buf.len() as u64;
        loop {
            let n = sock.read(&mut scratch).await?;
            if n == 0 {
                break;
            }
            body_bytes += n as u64;
        }
        return Ok(Response {
            status,
            ttfb,
            body_bytes,
            close: true,
        });
    }
    Ok(Response {
        status,
        ttfb,
        body_bytes,
        close,
    })
}

fn eof(what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("closed mid-{what}"),
    )
}

async fn read_more(sock: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut chunk = [0u8; 16 * 1024];
    let n = sock.read(&mut chunk).await?;
    buf.extend_from_slice(&chunk[..n]);
    Ok(n)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn request_bytes(opts: &Options, target: &Target, rng: &mut Rng) -> Vec<u8> {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: tanod-loadgen\r\nAccept: */*\r\n{}\r\n",
        render_path(&target.path, rng),
        opts.host,
        target.headers
    )
    .into_bytes()
}

fn pick(targets: &[Target], total: u64, rng: &mut Rng) -> usize {
    let mut roll = rng.below(total);
    for (i, t) in targets.iter().enumerate() {
        if roll < u64::from(t.weight) {
            return i;
        }
        roll -= u64::from(t.weight);
    }
    targets.len() - 1
}

async fn connection_loop(
    opts: Arc<Options>,
    id: u64,
    measure_from: Instant,
    stop_at: Instant,
    stopping: Arc<AtomicBool>,
) -> Tally {
    let mut tally = Tally::default();
    let mut rng =
        Rng(0x9E37_79B9_7F4A_7C15 ^ (id.wrapping_add(1).wrapping_mul(0xBF58_476D_1CE4_E5B9)));
    let total_weight: u64 = opts.targets.iter().map(|t| u64::from(t.weight)).sum();
    let mut buf = Vec::with_capacity(32 * 1024);
    let mut sock: Option<TcpStream> = None;
    while Instant::now() < stop_at && !stopping.load(Ordering::Relaxed) {
        let target = pick(&opts.targets, total_weight, &mut rng);
        let request = request_bytes(&opts, &opts.targets[target], &mut rng);
        let started = Instant::now();
        let result = async {
            if sock.is_none() {
                let s = TcpStream::connect(&opts.addr).await?;
                s.set_nodelay(true)?;
                sock = Some(s);
            }
            let s = sock
                .as_mut()
                .ok_or_else(|| std::io::Error::other("no socket"))?;
            exchange(s, &request, &mut buf, 0).await
        };
        let outcome = tokio::time::timeout(opts.timeout, result).await;
        let elapsed = started.elapsed();
        let recording = started >= measure_from && Instant::now() <= stop_at;
        match outcome {
            Ok(Ok(resp)) => {
                if resp.close {
                    sock = None;
                }
                if recording {
                    tally.bytes += resp.body_bytes;
                    tally.samples.push(Sample {
                        target,
                        slow: false,
                        total_us: micros(elapsed),
                        ttfb_us: micros(resp.ttfb),
                        status: resp.status,
                    });
                }
            }
            Ok(Err(e)) => {
                sock = None;
                if recording {
                    tally.errors += 1;
                    if tally.error_kinds.len() < 4 {
                        tally.error_kinds.push(e.to_string());
                    }
                }
            }
            Err(_) => {
                sock = None;
                if recording {
                    tally.errors += 1;
                    if tally.error_kinds.len() < 4 {
                        tally.error_kinds.push("timeout".into());
                    }
                }
            }
        }
    }
    tally
}

fn micros(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// CPU nanoseconds summed over every thread of a process, from the kernel's
/// scheduler statistics. Threads that exited are not counted; Tanod's workers
/// live as long as the process.
fn cpu_ns(pid: u32) -> Option<u64> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut total = 0u64;
    for task in tasks.flatten() {
        let path = task.path().join("schedstat");
        if let Ok(text) = std::fs::read_to_string(path) {
            total += text
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    Some(total)
}

fn status_kb(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

struct Watch {
    pid: u32,
    cpu_start: u64,
    rss_peak: Arc<AtomicU64>,
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn summary(values: &mut [u32]) -> String {
    values.sort_unstable();
    let mean = if values.is_empty() {
        0.0
    } else {
        values.iter().map(|v| f64::from(*v)).sum::<f64>() / values.len() as f64
    };
    format!(
        "{{\"p50\":{},\"p90\":{},\"p95\":{},\"p99\":{},\"p999\":{},\"max\":{},\"mean\":{:.1}}}",
        percentile(values, 50.0),
        percentile(values, 90.0),
        percentile(values, 95.0),
        percentile(values, 99.0),
        percentile(values, 99.9),
        values.last().copied().unwrap_or(0),
        mean
    )
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn start_watches(pids: &[u32], stop: Arc<AtomicBool>) -> Vec<Watch> {
    pids.iter()
        .map(|&pid| {
            let peak = Arc::new(AtomicU64::new(status_kb(pid, "VmRSS:").unwrap_or(0)));
            let sampler = peak.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(rss) = status_kb(pid, "VmRSS:") {
                        sampler.fetch_max(rss, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            Watch {
                pid,
                cpu_start: cpu_ns(pid).unwrap_or(0),
                rss_peak: peak,
            }
        })
        .collect()
}

fn finish_watches(watches: &[Watch], window: Duration) -> String {
    let rendered: Vec<String> = watches
        .iter()
        .map(|w| {
            let cpu = cpu_ns(w.pid).unwrap_or(0).saturating_sub(w.cpu_start) as f64 / 1e9;
            format!(
                "{{\"pid\":{},\"cpu_s\":{:.3},\"cpu_cores\":{:.3},\"rss_kb_peak_window\":{},\"rss_kb_end\":{},\"hwm_kb\":{}}}",
                w.pid,
                cpu,
                cpu / window.as_secs_f64().max(1e-9),
                w.rss_peak.load(Ordering::Relaxed),
                status_kb(w.pid, "VmRSS:").unwrap_or(0),
                status_kb(w.pid, "VmHWM:").unwrap_or(0),
            )
        })
        .collect();
    format!("[{}]", rendered.join(","))
}

fn report(opts: &Options, tallies: Vec<Tally>, window: Duration, watches: String) {
    let mut samples = Vec::new();
    let mut errors = 0;
    let mut kinds = Vec::new();
    let mut bytes = 0;
    for t in tallies {
        samples.extend(t.samples);
        errors += t.errors;
        bytes += t.bytes;
        for k in t.error_kinds {
            if kinds.len() < 4 && !kinds.contains(&k) {
                kinds.push(k);
            }
        }
    }
    let mut classes = [0u64; 6];
    for s in &samples {
        classes[usize::from(s.status / 100).min(5)] += 1;
    }
    let mut total: Vec<u32> = samples.iter().map(|s| s.total_us).collect();
    let mut ttfb: Vec<u32> = samples.iter().map(|s| s.ttfb_us).collect();
    let per_target: Vec<String> = opts
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut lat: Vec<u32> = samples
                .iter()
                .filter(|s| s.target == i)
                .map(|s| s.total_us)
                .collect();
            let non2xx = samples
                .iter()
                .filter(|s| s.target == i && !(200..300).contains(&s.status))
                .count();
            format!(
                "{{\"path\":\"{}\",\"weight\":{},\"requests\":{},\"non_2xx\":{},\"latency_us\":{}}}",
                json_escape(&t.path),
                t.weight,
                lat.len(),
                non2xx,
                summary(&mut lat)
            )
        })
        .collect();
    // With slow readers in a burst, the question is whether they held back
    // everybody else, so the two populations are reported apart.
    let split = if opts.slow_readers > 0 {
        let mut fast: Vec<u32> = samples
            .iter()
            .filter(|s| !s.slow)
            .map(|s| s.total_us)
            .collect();
        let mut fast_ttfb: Vec<u32> = samples
            .iter()
            .filter(|s| !s.slow)
            .map(|s| s.ttfb_us)
            .collect();
        let mut slow: Vec<u32> = samples
            .iter()
            .filter(|s| s.slow)
            .map(|s| s.total_us)
            .collect();
        format!(
            ",\"fast_latency_us\":{},\"fast_ttfb_us\":{},\"slow_latency_us\":{}",
            summary(&mut fast),
            summary(&mut fast_ttfb),
            summary(&mut slow)
        )
    } else {
        String::new()
    };
    let completed = samples.len() as u64;
    println!(
        "{{\"label\":\"{}\",\"mode\":\"{}\",\"connections\":{},\"threads\":{},\"window_s\":{:.3},\
         \"requests\":{},\"rps\":{:.1},\"errors\":{},\"error_kinds\":[{}],\
         \"status\":{{\"1xx\":{},\"2xx\":{},\"3xx\":{},\"4xx\":{},\"5xx\":{}}},\"body_bytes\":{},\
         \"latency_us\":{},\"ttfb_us\":{}{},\"targets\":[{}],\"processes\":{}}}",
        json_escape(&opts.label),
        opts.mode,
        opts.connections,
        opts.threads,
        window.as_secs_f64(),
        completed,
        completed as f64 / window.as_secs_f64().max(1e-9),
        errors,
        kinds
            .iter()
            .map(|k| format!("\"{}\"", json_escape(k)))
            .collect::<Vec<_>>()
            .join(","),
        classes[1],
        classes[2],
        classes[3],
        classes[4],
        classes[5],
        bytes,
        summary(&mut total),
        summary(&mut ttfb),
        split,
        per_target.join(","),
        watches
    );
}

async fn run(opts: Arc<Options>) {
    let started = Instant::now();
    let measure_from = started + opts.warmup;
    let stop_at = measure_from + opts.duration;
    let stopping = Arc::new(AtomicBool::new(false));
    let watch_stop = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..opts.connections)
        .map(|id| {
            tokio::spawn(connection_loop(
                opts.clone(),
                id as u64,
                measure_from,
                stop_at,
                stopping.clone(),
            ))
        })
        .collect();
    tokio::time::sleep_until(measure_from.into()).await;
    let watches = start_watches(&opts.watch, watch_stop.clone());
    let window_start = Instant::now();
    tokio::time::sleep_until(stop_at.into()).await;
    let window = window_start.elapsed();
    let watched = finish_watches(&watches, window);
    watch_stop.store(true, Ordering::Relaxed);
    stopping.store(true, Ordering::Relaxed);
    let mut tallies = Vec::new();
    for h in handles {
        // A connection still waiting on a slow response is abandoned rather
        // than awaited: its request started inside the window but finished
        // outside it, and is not recorded either way.
        if let Ok(Ok(t)) = tokio::time::timeout(Duration::from_millis(200), h).await {
            tallies.push(t);
        }
    }
    report(&opts, tallies, window, watched);
}

async fn burst(opts: Arc<Options>) {
    let mut socks = Vec::with_capacity(opts.connections);
    for _ in 0..opts.connections {
        match TcpStream::connect(&opts.addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                socks.push(s);
            }
            Err(e) => {
                eprintln!("loadgen: connect failed after {} sockets: {e}", socks.len());
                std::process::exit(1);
            }
        }
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(opts.connections + 1));
    let watch_stop = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = socks
        .into_iter()
        .enumerate()
        .map(|(i, mut sock)| {
            let opts = opts.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1));
                let total_weight: u64 = opts.targets.iter().map(|t| u64::from(t.weight)).sum();
                let target = pick(&opts.targets, total_weight, &mut rng);
                let request = request_bytes(&opts, &opts.targets[target], &mut rng);
                let slow = if i < opts.slow_readers {
                    opts.slow_bps
                } else {
                    0
                };
                let mut buf = Vec::with_capacity(32 * 1024);
                barrier.wait().await;
                let started = Instant::now();
                let outcome = tokio::time::timeout(
                    opts.timeout,
                    exchange(&mut sock, &request, &mut buf, slow),
                )
                .await;
                let elapsed = started.elapsed();
                let mut tally = Tally::default();
                match outcome {
                    Ok(Ok(resp)) => {
                        tally.bytes = resp.body_bytes;
                        tally.samples.push(Sample {
                            target,
                            slow: slow > 0,
                            total_us: micros(elapsed),
                            ttfb_us: micros(resp.ttfb),
                            status: resp.status,
                        });
                    }
                    Ok(Err(e)) => {
                        tally.errors = 1;
                        tally.error_kinds.push(e.to_string());
                    }
                    Err(_) => {
                        tally.errors = 1;
                        tally.error_kinds.push("timeout".into());
                    }
                }
                tally
            })
        })
        .collect();
    let watches = start_watches(&opts.watch, watch_stop.clone());
    let window_start = Instant::now();
    barrier.wait().await;
    let mut tallies = Vec::new();
    for h in handles {
        if let Ok(t) = h.await {
            tallies.push(t);
        }
    }
    let window = window_start.elapsed();
    let watched = finish_watches(&watches, window);
    watch_stop.store(true, Ordering::Relaxed);
    report(&opts, tallies, window, watched);
}

fn main() {
    let opts = Arc::new(parse_options());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(opts.threads.max(1))
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("loadgen: runtime: {e}");
            std::process::exit(1);
        });
    runtime.block_on(async move {
        if opts.mode == "burst" {
            burst(opts).await;
        } else {
            run(opts).await;
        }
    });
}
