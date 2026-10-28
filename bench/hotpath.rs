//! Hot-path microbenchmarks: the cache store, route resolution and cache-key
//! construction, measured in process with no network in the way.
//!
//! These isolate what a full-system run cannot: whether the store's locks
//! scale with concurrent readers and writers, what a cache-hit read costs per
//! body size, and how route resolution grows with the route table. They are
//! not a substitute for `bench/perf.sh`, which measures the proxy end to end.
//!
//! `cargo bench --bench hotpath` runs every scenario for `HOTPATH_SECS`
//! (default 1.0) and prints one JSON object per line. Under `cargo test`
//! (no `--bench` argument) each scenario runs a handful of iterations, so the
//! target keeps compiling and running without slowing the test suite.
//!
//! `HOTPATH_ONLY=<prefix>` restricts the run to scenarios whose name starts
//! with it, and `HOTPATH_OPS=<n>` replaces the timed loop with exactly `n`
//! operations on one thread, so an external allocation profiler (valgrind
//! DHAT) can divide its totals by a known count.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use pingora_cache::CacheMeta;
use pingora_cache::key::CacheKey;
use pingora_cache::storage::Storage;
use pingora_cache::trace::Span;
use pingora_http::ResponseHeader;
use tanod::cache::KeyBuilder;
use tanod::cache::store::BoundedStore;
use tanod::classifier::RequestMetadata;
use tanod::config::schema::{CacheDefaults, Config, Eviction};
use tanod::config::units::Bytes as ByteSize;
use tanod::policy::PolicySnapshot;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct Settings {
    secs: f64,
    full: bool,
    only: Option<String>,
    ops: Option<u64>,
}

impl Settings {
    fn from_env() -> Self {
        let full = std::env::args().any(|a| a == "--bench");
        Settings {
            secs: std::env::var("HOTPATH_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0),
            full,
            only: std::env::var("HOTPATH_ONLY").ok().filter(|s| !s.is_empty()),
            ops: std::env::var("HOTPATH_OPS")
                .ok()
                .and_then(|s| s.parse().ok()),
        }
    }

    fn wants(&self, name: &str) -> bool {
        self.only.as_deref().is_none_or(|p| name.starts_with(p))
    }
}

/// A response header shaped like a real SSR document's.
fn ssr_meta(body_len: usize) -> Result<CacheMeta, BoxError> {
    let mut header = ResponseHeader::build(200, Some(10))?;
    header.insert_header("content-type", "text/html; charset=utf-8")?;
    header.insert_header("content-length", body_len.to_string())?;
    header.insert_header(
        "cache-control",
        "private, no-cache, no-store, max-age=0, must-revalidate",
    )?;
    header.insert_header("date", "Wed, 28 Oct 2026 07:49:04 GMT")?;
    header.insert_header("etag", "\"a3f9c2e1b7d04f6a\"")?;
    header.insert_header(
        "vary",
        "RSC, Next-Router-State-Tree, Next-Router-Prefetch, Accept-Encoding",
    )?;
    header.insert_header("x-powered-by", "Next.js")?;
    header.insert_header("x-nextjs-cache", "MISS")?;
    let now = SystemTime::now();
    Ok(CacheMeta::new(
        now + Duration::from_secs(3600),
        now,
        0,
        0,
        header,
    ))
}

fn store(max: usize, eviction: Eviction) -> &'static BoundedStore {
    BoundedStore::new(&CacheDefaults {
        max_memory: ByteSize(max as u64),
        max_body_size: ByteSize((max / 2) as u64),
        eviction,
        ..Default::default()
    })
}

/// What one operation came to. A refused or aborted fill is the store doing
/// its job under memory pressure (the proxy serves those responses uncached
/// or aborts them visibly), so it is counted rather than treated as a fault.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    /// Registration refused: Pingora would serve this response uncached.
    Refused,
    /// A body write failed past the headroom: Pingora would abort it.
    Aborted,
}

/// One fill, as Pingora drives it: register, stream the body in chunks with
/// the task yielding between them (where Pingora awaits the next upstream
/// read), then finish.
async fn fill(
    store: &'static BoundedStore,
    key: &CacheKey,
    body: &Bytes,
    chunk: usize,
) -> Result<Outcome, BoxError> {
    let trace = Span::inactive().handle();
    let Ok(mut writer) = store
        .get_miss_handler(key, &ssr_meta(body.len())?, &trace)
        .await
    else {
        return Ok(Outcome::Refused);
    };
    let mut at = 0;
    while at < body.len() {
        let end = (at + chunk).min(body.len());
        if writer
            .write_body(body.slice(at..end), end == body.len())
            .await
            .is_err()
        {
            return Ok(Outcome::Aborted);
        }
        at = end;
        tokio::task::yield_now().await;
    }
    if body.is_empty() {
        writer.write_body(Bytes::new(), true).await?;
    }
    writer.finish().await?;
    Ok(Outcome::Done)
}

/// One cache hit, read to the end: what Pingora does for every hit.
async fn hit(store: &'static BoundedStore, key: &CacheKey) -> Result<Option<usize>, BoxError> {
    let trace = Span::inactive().handle();
    let Some((_meta, mut handler)) = store.lookup(key, &trace).await? else {
        return Ok(None);
    };
    let mut read = 0;
    while let Some(chunk) = handler.read_body().await? {
        read += chunk.len();
    }
    Ok(Some(read))
}

fn current_thread() -> tokio::runtime::Runtime {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => panic!("runtime: {e}"),
    }
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

#[derive(Default)]
struct Counts {
    done: u64,
    refused: u64,
    aborted: u64,
}

/// Run `concurrency` tasks of `op` on a multi-thread runtime with
/// `min(concurrency, cores)` workers — Tanod's own model, where concurrent
/// requests are tasks and only the worker threads can contend for a lock at
/// the same instant — for the configured duration.
///
/// The measured window is fixed before any task starts and every task checks
/// it itself, so a late-scheduled coordinator cannot shrink the window under
/// work it never saw begin.
fn timed<F, Fut>(settings: &Settings, concurrency: usize, op: F) -> (f64, Counts, usize)
where
    F: Fn(u64, u64) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Outcome, BoxError>> + Send + 'static,
{
    let workers = concurrency.min(cores()).max(1);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => panic!("runtime: {e}"),
    };
    let op = Arc::new(op);
    let budget = if settings.full { None } else { Some(20u64) };
    let window = Duration::from_secs_f64(if settings.full { settings.secs } else { 0.0 });
    let counts = rt.block_on(async {
        let opens = Instant::now() + Duration::from_millis(50);
        let closes = opens + window;
        let tasks: Vec<_> = (0..concurrency)
            .map(|t| {
                let op = op.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(opens.into()).await;
                    let mut counts = Counts::default();
                    let mut n = 0u64;
                    loop {
                        let open = match budget {
                            Some(b) => n < b,
                            None => Instant::now() < closes,
                        };
                        if !open {
                            break;
                        }
                        match op(t as u64, n).await {
                            Ok(Outcome::Done) => counts.done += 1,
                            Ok(Outcome::Refused) => counts.refused += 1,
                            Ok(Outcome::Aborted) => counts.aborted += 1,
                            Err(e) => panic!("operation failed: {e}"),
                        }
                        n += 1;
                    }
                    counts
                })
            })
            .collect();
        let mut total = Counts::default();
        for task in tasks {
            match task.await {
                Ok(c) => {
                    total.done += c.done;
                    total.refused += c.refused;
                    total.aborted += c.aborted;
                }
                Err(e) => panic!("benchmark task failed: {e}"),
            }
        }
        total
    });
    let all = counts.done + counts.refused + counts.aborted;
    (all as f64 / window.as_secs_f64().max(1e-9), counts, workers)
}

fn emit(scenario: &str, fields: &[(&str, String)]) {
    let rendered: Vec<String> = fields.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    println!("{{\"scenario\":\"{scenario}\",{}}}", rendered.join(","));
}

/// Exactly `ops` operations on this thread, for an external profiler.
fn counted<F, Fut>(ops: u64, op: F)
where
    F: Fn(u64, u64) -> Fut,
    Fut: std::future::Future<Output = Result<Outcome, BoxError>>,
{
    current_thread().block_on(async {
        for n in 0..ops {
            if let Err(e) = op(0, n).await {
                panic!("operation failed: {e}");
            }
        }
    });
}

const SIZES: [(usize, &str); 5] = [
    (1024, "1KiB"),
    (16 * 1024, "16KiB"),
    (64 * 1024, "64KiB"),
    (1024 * 1024, "1MiB"),
    (4 * 1024 * 1024, "4MiB"),
];
const CONCURRENCY: [usize; 5] = [1, 8, 32, 128, 512];
const HOT_KEYS: u64 = 64;

fn cache_hits(settings: &Settings) -> Result<(), BoxError> {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        let policy = format!("{eviction:?}").to_ascii_lowercase();
        for (size, label) in SIZES {
            let name = format!("hit/{policy}/{label}");
            if !settings.wants(&name) {
                continue;
            }
            let s = store((64 * (size + 4096)).max(64 << 20), eviction);
            let body = Bytes::from(vec![b'x'; size]);
            let keys: Arc<Vec<CacheKey>> = Arc::new(
                (0..HOT_KEYS)
                    .map(|i| CacheKey::new(format!("/hit/{i}"), format!("/hit/{i}")))
                    .collect(),
            );
            current_thread().block_on(async {
                for key in keys.iter() {
                    if fill(s, key, &body, 16 * 1024).await? != Outcome::Done {
                        return Err("could not fill the hot set".into());
                    }
                }
                Ok::<_, BoxError>(())
            })?;
            let op = {
                let keys = keys.clone();
                move |t: u64, n: u64| {
                    let key = keys[((t * 7 + n) % HOT_KEYS) as usize].clone();
                    async move {
                        match hit(s, &key).await? {
                            Some(_) => Ok(Outcome::Done),
                            None => Err("a hot key missed".into()),
                        }
                    }
                }
            };
            if let Some(ops) = settings.ops {
                counted(ops, op);
                emit(&name, &[("ops", ops.to_string())]);
                continue;
            }
            for concurrency in CONCURRENCY {
                if !settings.full && concurrency > 8 {
                    continue;
                }
                let (rate, counts, workers) = timed(settings, concurrency, op.clone());
                emit(
                    &name,
                    &[
                        ("concurrency", concurrency.to_string()),
                        ("workers", workers.to_string()),
                        ("ops_per_s", format!("{rate:.0}")),
                        ("ops", counts.done.to_string()),
                        (
                            "gib_per_s",
                            format!("{:.2}", rate * size as f64 / (1u64 << 30) as f64),
                        ),
                    ],
                );
            }
        }
    }
    Ok(())
}

/// Distinct keys filling a cache much smaller than the working set, so every
/// fill also evicts: the reservation path under steady pressure. A share of
/// readers hit the hot set at the same time, which is what turns writer lock
/// hold time into reader latency.
fn cache_fills(settings: &Settings) -> Result<(), BoxError> {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        let policy = format!("{eviction:?}").to_ascii_lowercase();
        for (readers_per_writer, label) in [(0u64, "fill-only"), (9, "fill-10pct")] {
            let name = format!("fill/{policy}/{label}");
            if !settings.wants(&name) {
                continue;
            }
            let s = store(8 << 20, eviction);
            let body = Bytes::from(vec![b'y'; 16 * 1024]);
            let hot: Arc<Vec<CacheKey>> = Arc::new(
                (0..HOT_KEYS)
                    .map(|i| CacheKey::new(format!("/hot/{i}"), format!("/hot/{i}")))
                    .collect(),
            );
            let sequence = Arc::new(AtomicU64::new(0));
            let op = {
                let (hot, body, sequence) = (hot.clone(), body.clone(), sequence.clone());
                move |t: u64, n: u64| {
                    let (hot, body, sequence) = (hot.clone(), body.clone(), sequence.clone());
                    async move {
                        let slot = (t + n) % (readers_per_writer + 1);
                        let key = &hot[((t * 7 + n) % HOT_KEYS) as usize];
                        if slot == 0 || readers_per_writer == 0 {
                            let id = sequence.fetch_add(1, Ordering::Relaxed);
                            let key = CacheKey::new(format!("/cold/{id}"), format!("/cold/{id}"));
                            fill(s, &key, &body, 4 * 1024).await
                        } else if hit(s, key).await?.is_some() {
                            Ok(Outcome::Done)
                        } else {
                            // Evicted by the fills: put it back, as a miss would.
                            fill(s, key, &body, 4 * 1024).await
                        }
                    }
                }
            };
            if let Some(ops) = settings.ops {
                counted(ops, op);
                emit(&name, &[("ops", ops.to_string())]);
                continue;
            }
            for concurrency in CONCURRENCY {
                if !settings.full && concurrency > 8 {
                    continue;
                }
                let (rate, counts, workers) = timed(settings, concurrency, op.clone());
                emit(
                    &name,
                    &[
                        ("concurrency", concurrency.to_string()),
                        ("workers", workers.to_string()),
                        ("ops_per_s", format!("{rate:.0}")),
                        ("done", counts.done.to_string()),
                        ("refused", counts.refused.to_string()),
                        ("aborted", counts.aborted.to_string()),
                        ("bytes_used", s.bytes_used().to_string()),
                        ("pinned", s.bytes_pinned().to_string()),
                    ],
                );
            }
        }
    }
    Ok(())
}

/// Re-filling keys that are already stored, which is what every expiry of a
/// short-TTL microcache does: the replacement has to retire the old entry's
/// bookkeeping, and how that scales with the number of entries is the
/// question.
fn cache_refills(settings: &Settings) -> Result<(), BoxError> {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        let policy = format!("{eviction:?}").to_ascii_lowercase();
        for entries in [1_000u64, 10_000, 100_000] {
            let name = format!("refill/{policy}/{entries}");
            if !settings.wants(&name) || (!settings.full && entries > 1_000) {
                continue;
            }
            let s = store(1 << 30, eviction);
            let body = Bytes::from(vec![b'r'; 1024]);
            let keys: Arc<Vec<CacheKey>> = Arc::new(
                (0..entries)
                    .map(|i| CacheKey::new(format!("/refill/{i}"), format!("/refill/{i}")))
                    .collect(),
            );
            current_thread().block_on(async {
                for key in keys.iter() {
                    if fill(s, key, &body, 1024).await? != Outcome::Done {
                        return Err::<(), BoxError>("could not fill".into());
                    }
                }
                Ok(())
            })?;
            let op = {
                let (keys, body) = (keys.clone(), body.clone());
                move |t: u64, n: u64| {
                    let (keys, body) = (keys.clone(), body.clone());
                    async move {
                        let i =
                            (t.wrapping_mul(0x9E37_79B9) ^ n.wrapping_mul(2_654_435_761)) % entries;
                        let i = usize::try_from(i).unwrap_or(0);
                        fill(s, &keys[i], &body, 1024).await
                    }
                }
            };
            if let Some(ops) = settings.ops {
                counted(ops, op);
                emit(&name, &[("ops", ops.to_string())]);
                continue;
            }
            let (rate, counts, _) = timed(settings, 1, op);
            emit(
                &name,
                &[
                    ("entries", entries.to_string()),
                    ("ns_per_op", format!("{:.1}", 1e9 / rate.max(1e-9))),
                    ("ops", counts.done.to_string()),
                    ("stored", s.entries().to_string()),
                ],
            );
        }
    }
    Ok(())
}

/// One leader streaming a 1 MiB body in 16 KiB chunks to `followers` readers
/// attached while it is in progress: the coalesced-herd shape.
fn followers(settings: &Settings) -> Result<(), BoxError> {
    for followers in [1usize, 10, 100, 500] {
        let name = format!("follow/{followers}");
        if !settings.wants(&name) || (!settings.full && followers > 10) {
            continue;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let rounds = if settings.full { 20 } else { 1 };
        let mut elapsed = Vec::new();
        for round in 0..rounds {
            let s = store(256 << 20, Eviction::Clock);
            let key = CacheKey::new(format!("/follow/{round}"), "/follow".to_string());
            let started = Instant::now();
            rt.block_on(async {
                let trace = Span::inactive().handle();
                let mut writer = s
                    .get_miss_handler(&key, &ssr_meta(1 << 20)?, &trace)
                    .await?;
                let mut readers = Vec::new();
                for _ in 0..followers {
                    let (_, mut handler) = s
                        .lookup(&key, &trace)
                        .await?
                        .ok_or("follower could not attach")?;
                    readers.push(tokio::spawn(async move {
                        let mut read = 0usize;
                        while let Some(chunk) = handler.read_body().await? {
                            read += chunk.len();
                        }
                        Ok::<_, BoxError>(read)
                    }));
                }
                let chunk = Bytes::from(vec![b'z'; 16 * 1024]);
                for i in 0..64 {
                    writer.write_body(chunk.clone(), i == 63).await?;
                    tokio::task::yield_now().await;
                }
                writer.finish().await?;
                for r in readers {
                    let read = r.await??;
                    if read != 1 << 20 {
                        return Err(format!("follower read {read} bytes").into());
                    }
                }
                Ok::<_, BoxError>(())
            })?;
            elapsed.push(started.elapsed().as_secs_f64() * 1e3);
        }
        elapsed.sort_by(f64::total_cmp);
        emit(
            &name,
            &[
                ("rounds", rounds.to_string()),
                ("median_ms", format!("{:.3}", elapsed[elapsed.len() / 2])),
                ("max_ms", format!("{:.3}", elapsed[elapsed.len() - 1])),
            ],
        );
    }
    Ok(())
}

fn routes_yaml(count: usize) -> String {
    let mut yaml = String::from("version: 1\norigin:\n  upstreams: [\"127.0.0.1:1\"]\nroutes:\n");
    // A realistic mix: static assets and API prefixes first, then exact
    // pages, then parameterised sections, then a catch-all.
    yaml.push_str("  - id: next-static\n    match: \"/_next/static/**\"\n");
    yaml.push_str("  - id: api\n    match: \"/api/**\"\n");
    for i in 0..count.saturating_sub(3) {
        match i % 3 {
            0 => yaml.push_str(&format!("  - id: page-{i}\n    match: \"/page-{i}\"\n")),
            1 => yaml.push_str(&format!(
                "  - id: section-{i}\n    match: \"/section-{i}/**\"\n    class: public_ssr\n"
            )),
            _ => yaml.push_str(&format!(
                "  - id: item-{i}\n    match:\n      path: \"/items-{i}/*/detail\"\n      methods: [GET, HEAD]\n"
            )),
        }
    }
    yaml.push_str("  - id: catchall\n    match: \"/**\"\n");
    yaml
}

fn route_matching(settings: &Settings) -> Result<(), BoxError> {
    for count in [5usize, 25, 100, 500, 1000] {
        let config: Config = serde_saphyr::from_str(&routes_yaml(count))?;
        let snapshot = PolicySnapshot::build(config, 1)?;
        let last_section = (0..count.saturating_sub(3))
            .rev()
            .find(|i| i % 3 == 1)
            .map(|i| format!("/section-{i}/deep/page"))
            .unwrap_or_else(|| "/api/x".into());
        for (position, path) in [
            ("first", "/_next/static/chunks/app.js".to_string()),
            ("last-section", last_section),
            ("catchall", "/unmatched/route/path".to_string()),
        ] {
            let name = format!("route/{count}/{position}");
            if !settings.wants(&name) {
                continue;
            }
            let snapshot = snapshot.clone();
            let op = move |_t: u64, _n: u64| {
                let found = snapshot
                    .resolve("shop.example.com", &path, &Method::GET)
                    .is_some();
                async move {
                    if found {
                        Ok(Outcome::Done)
                    } else {
                        Err("no route".into())
                    }
                }
            };
            if let Some(ops) = settings.ops {
                counted(ops, op);
                emit(&name, &[("ops", ops.to_string())]);
                continue;
            }
            let (rate, counts, _) = timed(settings, 1, op);
            emit(
                &name,
                &[
                    ("routes", count.to_string()),
                    ("ns_per_op", format!("{:.1}", 1e9 / rate.max(1e-9))),
                    ("ops", counts.done.to_string()),
                ],
            );
        }
    }
    Ok(())
}

/// Cache-key construction as `cache_key_callback` performs it, including
/// Pingora's hash of the result.
fn key_building(settings: &Settings) -> Result<(), BoxError> {
    let name = "key/public-ssr";
    if !settings.wants(name) {
        return Ok(());
    }
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("shop.example.com"));
    headers.insert(
        "accept-encoding",
        HeaderValue::from_static("gzip, deflate, br, zstd"),
    );
    headers.insert(
        "accept",
        HeaderValue::from_static("text/html,application/xhtml+xml"),
    );
    headers.insert(
        "user-agent",
        HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64)"),
    );
    let headers = Arc::new(headers);
    let variants: Arc<Vec<String>> = Arc::new(
        [
            "next-router-prefetch",
            "next-router-segment-prefetch",
            "next-router-state-tree",
            "rsc",
        ]
        .map(String::from)
        .to_vec(),
    );
    let op = move |_t: u64, _n: u64| {
        let meta = RequestMetadata {
            method: &Method::GET,
            host: "shop.example.com",
            path: "/products/iphone-15",
            query: Some("ref=home&utm_source=x"),
            headers: &headers,
        };
        let key = KeyBuilder {
            scheme: "https",
            forwarded_host: None,
            query_policy: None,
            variant_headers: &variants,
            deployment: Some("build-42"),
        }
        .build(&meta);
        let pingora = CacheKey::new(
            format!("tanod-policy-v1:1:{}", key.canonical_string()),
            "/products/iphone-15",
        );
        use pingora_cache::key::CacheHashKey;
        let hashed = pingora.combined();
        async move {
            if hashed.is_empty() {
                Err("empty".into())
            } else {
                Ok(Outcome::Done)
            }
        }
    };
    if let Some(ops) = settings.ops {
        counted(ops, op);
        emit(name, &[("ops", ops.to_string())]);
        return Ok(());
    }
    let (rate, counts, _) = timed(settings, 1, op);
    emit(
        name,
        &[
            ("ns_per_op", format!("{:.1}", 1e9 / rate.max(1e-9))),
            ("ops", counts.done.to_string()),
        ],
    );
    Ok(())
}

/// What producing a hit's `CacheMeta` costs, apart from the store: decoding
/// both serialized halves (what every hit pays today), against decoding only
/// the small internal half and cloning an already-decoded header.
fn meta_decoding(settings: &Settings) -> Result<(), BoxError> {
    let (internal, header) = ssr_meta(16 * 1024)?.serialize()?;
    let decoded = CacheMeta::deserialize(&internal, &header)?
        .response_header()
        .clone();
    let (_, tiny) = CacheMeta::new(
        SystemTime::now(),
        SystemTime::now(),
        0,
        0,
        ResponseHeader::build(200, Some(0))?,
    )
    .serialize()?;
    let (internal, header, decoded, tiny) = (
        Arc::new(internal),
        Arc::new(header),
        Arc::new(decoded),
        Arc::new(tiny),
    );
    for name in ["meta/deserialize", "meta/internal+clone"] {
        if !settings.wants(name) {
            continue;
        }
        let (internal, header, decoded, tiny) = (
            internal.clone(),
            header.clone(),
            decoded.clone(),
            tiny.clone(),
        );
        let clone_path = name.ends_with("clone");
        let (header_len, internal_len) = (header.len(), internal.len());
        let op = move |_t: u64, _n: u64| {
            let meta = if clone_path {
                CacheMeta::deserialize(&internal, &tiny).map(|mut m| {
                    *m.response_header_mut() = (*decoded).clone();
                    m
                })
            } else {
                CacheMeta::deserialize(&internal, &header)
            };
            async move {
                match meta {
                    Ok(m) if m.response_header().headers.len() > 1 || !clone_path => {
                        Ok(Outcome::Done)
                    }
                    Ok(_) => Err("header not replaced".into()),
                    Err(e) => Err(e.to_string().into()),
                }
            }
        };
        if let Some(ops) = settings.ops {
            counted(ops, op);
            emit(name, &[("ops", ops.to_string())]);
            continue;
        }
        let (rate, counts, _) = timed(settings, 1, op);
        emit(
            name,
            &[
                ("ns_per_op", format!("{:.1}", 1e9 / rate.max(1e-9))),
                ("ops", counts.done.to_string()),
                ("serialized_header_bytes", header_len.to_string()),
                ("internal_bytes", internal_len.to_string()),
            ],
        );
    }
    Ok(())
}

/// Hold `HOTPATH_FOOTPRINT` separately decoded copies of one SSR header, the
/// way stored entries would each hold their own, so DHAT's peak live bytes
/// give the heap cost of one. Only when asked for by name.
fn meta_footprint(settings: &Settings) -> Result<(), BoxError> {
    if settings.only.as_deref() != Some("meta/footprint") {
        return Ok(());
    }
    let count: usize = std::env::var("HOTPATH_FOOTPRINT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(10_000);
    let (internal, header) = ssr_meta(16 * 1024)?.serialize()?;
    let mut held = Vec::with_capacity(count);
    for _ in 0..count {
        held.push(CacheMeta::deserialize(&internal, &header)?.response_header_copy());
    }
    emit(
        "meta/footprint",
        &[
            ("headers", held.len().to_string()),
            ("fields", held[0].headers.len().to_string()),
            (
                "name_value_bytes",
                held[0]
                    .headers
                    .iter()
                    .map(|(n, v)| n.as_str().len() + v.len())
                    .sum::<usize>()
                    .to_string(),
            ),
        ],
    );
    Ok(())
}

fn main() -> Result<(), BoxError> {
    let settings = Settings::from_env();
    meta_footprint(&settings)?;
    meta_decoding(&settings)?;
    cache_hits(&settings)?;
    cache_fills(&settings)?;
    cache_refills(&settings)?;
    followers(&settings)?;
    route_matching(&settings)?;
    key_building(&settings)?;
    Ok(())
}
