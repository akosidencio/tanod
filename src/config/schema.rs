//! The `tanod.yaml` shape.
//!
//! Every struct is `deny_unknown_fields`. A typo'd key is a silent policy
//! change otherwise, and a silent policy change in *this* config means either
//! an unprotected origin or a cache serving something it should not.

use super::units::{Bytes, Dur};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// Rollout behavior. `observe` classifies and proxies without protecting
    /// or reusing responses; `protect` enables the configured controls.
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub server: Server,
    pub origin: Origin,
    /// Optional static partition of one origin-work budget across a bounded
    /// Tanod replica group.
    #[serde(default)]
    pub capacity: Option<CapacityGroup>,
    #[serde(default)]
    pub health: Option<Health>,
    #[serde(default)]
    pub cache: CacheDefaults,
    #[serde(default)]
    pub coalesce: CoalesceDefaults,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub overload: Overload,
    #[serde(default)]
    pub spool: Spool,
    #[serde(default)]
    pub upgrade: Upgrade,
    #[serde(default)]
    pub deployment: Deployment,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub telemetry: Telemetry,
    /// Emit the `X-Tanod` cache-status header. Off by default: it tells an
    /// attacker whether their probe was cached, which is reconnaissance for
    /// cache poisoning.
    #[serde(default)]
    pub debug_headers: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Observe,
    #[default]
    Protect,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityGroup {
    /// Maximum combined origin work across the declared replica group.
    pub global_max: usize,
    /// Maximum number of simultaneously active Tanod replicas.
    pub replicas: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Accept HTTP/2 over cleartext on `listen`.
    ///
    /// Pingora sniffs the connection preface, so an h2c listener still serves
    /// HTTP/1.1 clients. Off by default: h2c is only reachable by a client
    /// that already knows to speak it, and turning it on changes how request
    /// headers arrive (no `Host`, repeated `Cookie` lines) on a path that
    /// classification and cache keying both read.
    #[serde(default)]
    pub h2c: bool,
    /// Terminate TLS here rather than in front of Tanod.
    #[serde(default)]
    pub tls: Option<ServerTls>,
    /// Who is allowed to tell Tanod about the client.
    #[serde(default)]
    pub trusted_proxies: TrustedProxies,
    /// Process lifecycle: pid file, upgrade socket, drain and shutdown
    /// windows. These are what make the zero-downtime upgrade reachable.
    #[serde(default)]
    pub graceful: Graceful,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: default_listen(),
            h2c: false,
            tls: None,
            trusted_proxies: TrustedProxies::default(),
            graceful: Graceful::default(),
        }
    }
}

/// Zero-downtime upgrade and drain.
///
/// Pingora's `SIGQUIT` handoff passes the listening file descriptors to a new
/// process over `upgrade_socket`, so the old process keeps serving what it
/// already accepted while the new one takes every new connection. Both
/// processes must agree on the socket path, which is why it is configuration
/// rather than a constant: two Tanods on one host with the same default
/// would hand each other their listeners.
///
/// `drain_period` is separate from `shutdown_timeout` and does different work.
/// Draining is for the *load balancer*: readiness starts failing immediately,
/// and Tanod keeps serving normally for this long so the balancer has time
/// to notice and stop sending new work. Only then does the shutdown itself
/// begin, bounded by `shutdown_timeout`. Skipping the first window is the
/// usual cause of "we did a graceful restart and still dropped requests".
///
/// # `shutdown_timeout` is a floor, not a ceiling
///
/// This is the surprising one, and it is measured rather than assumed — see
/// `bench/upgrade.sh`. After asking each runtime to shut down, Pingora
/// deliberately sleeps for `shutdown_timeout`, so the wait runs to completion
/// whether or not anything is still in flight. **A `SIGTERM` therefore takes
/// about `drain_period + shutdown_timeout` every time, even on a completely
/// idle process.**
///
/// Two things follow, and both are ways deployments actually break:
///
/// * The defaults add up to 15 seconds, which fits inside Kubernetes'
///   default `terminationGracePeriodSeconds: 30` and systemd's default
///   `TimeoutStopSec=90`. Raise these two and you must raise those, or the
///   supervisor `SIGKILL`s Tanod part-way through the drain — dropping
///   exactly the requests the drain existed to protect.
/// * There is no point setting `shutdown_timeout` far above the longest
///   response you actually serve. It buys nothing and every restart pays it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graceful {
    /// Written by Pingora when `tanod run --daemon` starts. Foreground
    /// supervisors and containers should signal their tracked main process
    /// instead; Pingora does not create a pid file in foreground mode.
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    /// Unix socket the old and new processes use to pass listening fds.
    #[serde(default = "default_upgrade_socket")]
    pub upgrade_socket: String,
    /// How long Tanod keeps serving after readiness starts failing, before
    /// the shutdown proper begins. Gives a load balancer time to take this
    /// instance out of rotation.
    #[serde(default = "d_5s")]
    pub drain_period: Dur,
    /// How long in-flight requests get once the shutdown proper begins.
    /// Pingora accepts whole seconds, so Tanod rounds upward.
    ///
    /// Ten seconds rather than thirty: see the note above on this being a
    /// floor. A thirty-second value makes every ordinary restart take
    /// thirty-five seconds and puts the process past Kubernetes' default
    /// termination grace period, where it is `SIGKILL`ed mid-drain.
    #[serde(default = "d_10s")]
    pub shutdown_timeout: Dur,
}

impl Default for Graceful {
    fn default() -> Self {
        Graceful {
            pid_file: default_pid_file(),
            upgrade_socket: default_upgrade_socket(),
            drain_period: d_5s(),
            shutdown_timeout: d_10s(),
        }
    }
}

fn default_pid_file() -> String {
    "/tmp/tanod.pid".into()
}
fn default_upgrade_socket() -> String {
    "/tmp/tanod-upgrade.sock".into()
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTls {
    /// A second listener. `server.listen` keeps serving cleartext, so an
    /// operator can run both during a migration without a second process.
    pub listen: String,
    /// PEM certificate chain: leaf first, then intermediates.
    pub cert: String,
    /// PEM private key.
    pub key: String,
    /// Offer `h2` in ALPN. `http/1.1` is always offered as well, so a client
    /// that cannot speak HTTP/2 is never locked out.
    #[serde(default = "yes")]
    pub h2: bool,
}

/// Forwarded metadata is a claim, not a fact.
///
/// `X-Forwarded-For` and `X-Forwarded-Proto` are set by whoever spoke to us
/// last, and anyone on the internet can spoof both. Tanod therefore reads
/// them only from a peer whose address is in `from`. Everyone else is treated
/// as the client, whatever they claim — which is also why `from` is empty by
/// default: an unconfigured Tanod cannot be lied to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedProxies {
    /// CIDR blocks whose forwarded headers are believed. `10.0.0.0/8`,
    /// `192.168.1.7/32` and `2001:db8::/32` all parse; a bare address is
    /// treated as a single-host prefix.
    #[serde(default)]
    pub from: Vec<String>,
    /// Where the client address comes from when the peer is trusted.
    #[serde(default)]
    pub client_ip: ForwardedSource,
    /// Where the original scheme comes from when the peer is trusted.
    ///
    /// The scheme is part of the cache key, so a spoofable one is a cache
    /// partition an attacker controls.
    #[serde(default)]
    pub scheme: ForwardedSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardedSource {
    /// `X-Forwarded-For` / `X-Forwarded-Proto`.
    #[default]
    XForwarded,
    /// RFC 7239 `Forwarded: for=...;proto=...`.
    Forwarded,
    /// Believe nothing. The connection peer is the client and the listener
    /// decides the scheme.
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub upstreams: Vec<String>,
    /// How often each upstream name is looked up again, so instances the
    /// platform adds or replaces start (and stop) receiving traffic without a
    /// restart. `0` resolves once at startup, the old behaviour.
    #[serde(default = "d_10s")]
    pub resolve_interval: Dur,
    #[serde(default)]
    pub load_balancing: LoadBalancing,
    #[serde(default)]
    pub concurrency: Concurrency,
    /// Speak TLS to the origin.
    #[serde(default)]
    pub tls: Option<OriginTls>,
    /// Which HTTP version to speak to the origin.
    #[serde(default)]
    pub http_version: OriginHttpVersion,
    /// Passive failure observation and per-backend circuit breaking.
    #[serde(default)]
    pub breaker: Breaker,
    /// Bounded retries for requests that are safe to send twice.
    #[serde(default)]
    pub retry: Retry,
    /// How much of the global ceiling each route priority may occupy.
    #[serde(default)]
    pub priorities: Priorities,
}

/// Reserved capacity, expressed as what each priority is *allowed* rather
/// than what it is promised.
///
/// Each tier gets a ceiling of its own, as a percent of
/// `origin.concurrency.max`, and a request is admitted only if its tier has
/// room as well as the global limiter. Reserved capacity is the complement:
/// setting `low: 50` reserves half the origin ceiling for `normal` and
/// `high` work, because low-priority requests can never occupy more than the
/// other half no matter how many of them arrive.
///
/// Stating it as a ceiling rather than a reservation is what makes it
/// enforceable. A reservation has to be defended against work that already
/// holds the slot, which means preempting a render that is mid-flight; a
/// ceiling is checked before the work starts, when refusing is still free.
///
/// The default is 100 for every tier, which is the same behaviour as having
/// no tiers at all. Nothing changes until someone lowers one.
///
/// This does **not** reorder the queue. Within a tier, waiting is FIFO, and a
/// high-priority request that arrives behind a queue of its own peers waits
/// its turn. The isolation is between tiers, not inside one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Priorities {
    #[serde(default = "hundred")]
    pub high: u32,
    #[serde(default = "hundred")]
    pub normal: u32,
    #[serde(default = "hundred")]
    pub low: u32,
}

impl Default for Priorities {
    fn default() -> Self {
        Priorities {
            high: hundred(),
            normal: hundred(),
            low: hundred(),
        }
    }
}

impl Priorities {
    /// Is this the inert default, where every tier may use the whole ceiling?
    pub fn is_uniform(&self) -> bool {
        self.high == 100 && self.normal == 100 && self.low == 100
    }

    pub fn percent_for(&self, priority: Priority) -> u32 {
        match priority {
            Priority::High => self.high,
            Priority::Normal => self.normal,
            Priority::Low => self.low,
        }
    }
}

/// Which tier's ceiling a route's requests are admitted against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    /// Stable label for metrics and the status document. Prefixed because it
    /// shares the `limiter` label space with `global` and route ids.
    pub fn limiter_name(self) -> &'static str {
        match self {
            Priority::High => "tier:high",
            Priority::Normal => "tier:normal",
            Priority::Low => "tier:low",
        }
    }

    /// Every tier, in the order a status document should print them.
    pub const ALL: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Low];
}

/// Eject a backend that is failing *real* traffic.
///
/// An active health probe asks a backend whether it is alive on one path, at
/// one interval, with one request. It is deliberately cheap, and that is
/// exactly why it misses the failures that matter: a Node origin that answers
/// `/healthz` in a millisecond while every render 500s, a pod whose upstream
/// database connection died, a container that lost its CPU share. Passive
/// observation watches the requests Tanod is *already* sending and needs no
/// extra traffic to see any of that.
///
/// The breaker is per backend and additive: a backend must be passing its
/// health check **and** have a closed breaker to be preferred. Off by default
/// — with a single-backend origin there is nowhere to eject to, and ejecting
/// the only backend would convert a partial failure into a total one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Breaker {
    #[serde(default)]
    pub enabled: bool,
    /// The rolling window failures are counted over.
    #[serde(default = "d_10s")]
    pub window: Dur,
    /// Never trip on a handful of requests. A backend that saw three
    /// requests and failed two of them is a 67% failure rate and also
    /// nothing but noise.
    #[serde(default = "twenty")]
    pub min_requests: u32,
    /// Percent of requests in the window that must have failed. Integer
    /// percent rather than a ratio so the threshold means the same thing on
    /// every platform and reads the same in a config review.
    #[serde(default = "fifty")]
    pub failure_percent: u32,
    /// How long a tripped backend stays out. When it expires, exactly one
    /// request is allowed through as a probe; a success closes the breaker
    /// and a failure starts the window again.
    #[serde(default = "d_30s")]
    pub open_for: Dur,
    /// The outlier-ejection cap: the most of the pool that may be ejected at
    /// once, as a percent.
    ///
    /// This is the check that keeps a breaker from causing the outage it
    /// exists to contain. When an origin-wide dependency fails, *every*
    /// backend starts failing, every breaker trips, and a proxy that honoured
    /// all of them would have nowhere to send anything. Past this cap Tanod
    /// ignores breaker state entirely and goes back to health-based routing:
    /// if everything is broken, "broken" is no longer a reason to prefer one
    /// backend over another.
    #[serde(default = "fifty")]
    pub max_ejected_percent: u32,
}

impl Default for Breaker {
    fn default() -> Self {
        Breaker {
            enabled: false,
            window: d_10s(),
            min_requests: twenty(),
            failure_percent: fifty(),
            open_for: d_30s(),
            max_ejected_percent: fifty(),
        }
    }
}

/// Retrying a failed origin request, without amplifying an outage.
///
/// A retry is extra origin load applied at exactly the moment the origin is
/// least able to absorb it, so every part of this is a bound. Two of them are
/// not configurable because getting them wrong is not a tuning mistake:
///
/// * Only **safe** methods are ever retried — `GET`, `HEAD`, `OPTIONS`,
///   `TRACE`. `PUT` and `DELETE` are idempotent on paper, but Tanod does not
///   buffer request bodies and cannot replay one; and an origin that treats
///   `POST` as idempotent is not a bet a proxy gets to make on its behalf.
/// * A retry is only ever attempted **before the origin has answered** — a
///   connect failure, or an error on a reused keepalive connection where
///   nothing was written downstream. Once a byte of the response has been
///   sent, the request is finished whatever happens next.
///
/// The budget is what stops a retrying proxy from doubling the load on a
/// struggling origin: retries are capped as a fraction of the traffic
/// actually flowing, so a total outage — where every request fails — allows
/// almost no retries at all, while a single blip is absorbed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    #[serde(default)]
    pub enabled: bool,
    /// Total tries for one request, the first included. `2` means one retry.
    #[serde(default = "two")]
    pub max_attempts: u32,
    /// The rolling window the budget is measured over.
    #[serde(default = "d_10s")]
    pub window: Dur,
    /// Retries allowed as a percent of origin requests in the window.
    ///
    /// 10% is Envoy's default and a good starting point: it absorbs an
    /// individual backend dying while making it arithmetically impossible for
    /// retries to be more than a tenth of the load the origin sees.
    #[serde(default = "ten")]
    pub budget_percent: u32,
    /// Retries always allowed in a window regardless of the percentage, so a
    /// low-traffic deployment is not left with a budget that rounds to zero.
    #[serde(default = "three_usize")]
    pub budget_min: usize,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            enabled: false,
            max_attempts: two(),
            window: d_10s(),
            budget_percent: ten(),
            budget_min: three_usize(),
        }
    }
}

fn twenty() -> u32 {
    20
}
fn fifty() -> u32 {
    50
}
fn ten() -> u32 {
    10
}
fn two() -> u32 {
    2
}
fn three_usize() -> usize {
    3
}
fn hundred() -> u32 {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginTls {
    /// SNI and the name the certificate is checked against. Required: a peer
    /// with an empty SNI cannot be hostname-verified, and silently not
    /// verifying is the failure mode this key exists to prevent.
    pub sni: String,
    /// Verify the origin's certificate chain.
    ///
    /// `false` is accepted for a self-signed origin behind a private network,
    /// and is loud in `tanod check` because it turns the connection into
    /// encryption without authentication.
    #[serde(default = "yes")]
    pub verify_cert: bool,
    /// Also require the certificate to match `sni`.
    #[serde(default = "yes")]
    pub verify_hostname: bool,
    /// PEM bundle to trust in addition to the system roots.
    ///
    /// Accepted by the schema and **rejected at startup**: Pingora 0.8's
    /// rustls connector never reads the per-peer CA store — its `connect`
    /// carries a `TODO: setup CA/verify cert store from peer` and
    /// `peer.get_ca()` is unused. Silently ignoring it would mean a config
    /// that names a CA, a proxy that verifies against the system roots
    /// instead, and no way to tell from the outside. Use `SSL_CERT_FILE` /
    /// `SSL_CERT_DIR`, which the platform store does honour.
    #[serde(default)]
    pub ca: Option<String>,
}

/// HTTP/1.1 by default because that is what `next start` speaks.
///
/// `http2` over cleartext is prior-knowledge h2c: there is no ALPN to
/// negotiate with and no upgrade dance, so an origin that does not speak it
/// answers with a protocol error rather than falling back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginHttpVersion {
    #[default]
    Http1,
    Http2,
    /// Offer both in ALPN and take what the origin picks. Only meaningful with
    /// `origin.tls`; over cleartext there is nothing to negotiate with, so
    /// validation refuses the combination.
    Auto,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancing {
    #[default]
    RoundRobin,
    /// Route a given path to a consistent backend. Costs nothing and improves
    /// the *origin's* own warmth — its in-process render cache, module graph
    /// and JIT state are all path-correlated.
    HashByPath,
    /// Send each request to the backend with the least outstanding work,
    /// scored as in-flight requests against observed latency.
    ///
    /// Only worth choosing once there is something to observe. It is the
    /// strategy that notices a backend which is *answering* but slow — a
    /// Node process in a GC death spiral, a container that lost its CPU
    /// share — which neither a health probe nor round-robin can see.
    LeastLoaded,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    pub max: usize,
    #[serde(default)]
    pub queue: Queue,
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency {
            max: 500,
            queue: Queue::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    /// Hard bound. An unbounded queue converts an origin overload into a
    /// proxy overload and defers the failure instead of preventing it.
    pub max: usize,
    pub timeout: Dur,
}

impl Default for Queue {
    fn default() -> Self {
        Queue {
            max: 0,
            timeout: Dur::ZERO,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub path: String,
    pub interval: Dur,
    pub timeout: Dur,
    #[serde(default = "one")]
    pub healthy_after: u32,
    #[serde(default = "three")]
    pub unhealthy_after: u32,
}

fn one() -> u32 {
    1
}
fn three() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheDefaults {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub store: Store,
    #[serde(default = "default_max_memory")]
    pub max_memory: Bytes,
    #[serde(default = "default_max_body")]
    pub max_body_size: Bytes,
    /// Origin `Cache-Control` wins unless a route explicitly opts out. Global
    /// opt-out is not offered at any level; see `Route.cache.override_origin`.
    #[serde(default = "yes")]
    pub respect_origin: bool,
    /// Which entry is discarded when the byte budget is full.
    #[serde(default)]
    pub eviction: Eviction,
    /// Response header the origin uses to tag an entry for later invalidation.
    ///
    /// Framework-neutral on purpose: any origin that can set a header can be
    /// purged by tag, with no adapter and no JavaScript. Next.js emits its own
    /// `x-next-cache-tags` on statically generated App Router responses when
    /// the server runs in minimal mode, so pointing this at that header makes
    /// `revalidateTag()` reachable from Tanod without an integration — at
    /// the cost of depending on a private Next environment variable. See
    /// `docs/OPERATIONS.md`.
    ///
    /// Whatever it is named, Tanod strips it from the downstream response:
    /// tag names describe an origin's internal content model and are nobody
    /// else's business.
    #[serde(default = "default_tag_header")]
    pub tag_header: String,
    /// The purge endpoint, served on the admin listener.
    #[serde(default)]
    pub purge: Purge,
}

/// What gets discarded when the cache is full.
///
/// The distinction matters more than it looks. A microcache in front of an SSR
/// origin sees a heavily skewed request distribution — a handful of URLs are
/// most of the traffic — and FIFO evicts a hot entry as readily as a cold one
/// purely because it arrived earlier. `bench/eviction.rs` measures the
/// difference on a Zipfian workload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eviction {
    /// Second-chance FIFO. An entry that has been read since it was queued
    /// survives one pass and is requeued; the second time round it goes.
    ///
    /// The default. It costs one relaxed atomic store on the hit path and
    /// nothing else — no list surgery, no write lock on read — which is why it
    /// is affordable where true LRU is not.
    #[default]
    Clock,
    /// Strict insertion order, ignoring reads entirely. Kept so the
    /// improvement is measurable rather than asserted, and as an escape hatch
    /// if the extra atomic ever matters.
    Fifo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Purge {
    /// Shared secret required by `POST /purge`. **Without it the endpoint does
    /// not exist**, which is the only safe default: purging is the one admin
    /// operation that costs the origin real work, and an open purge endpoint
    /// is a cache-stampede trigger anybody can pull.
    ///
    /// The endpoint lives on the admin listener, so this is defence in depth
    /// rather than the only barrier. Both are wanted: the admin listener is
    /// bound wherever an operator points it, and read-only status is a very
    /// different exposure from invalidation.
    #[serde(default)]
    pub token: Option<String>,
}

fn default_tag_header() -> String {
    "x-tanod-cache-tags".into()
}

impl Default for CacheDefaults {
    fn default() -> Self {
        CacheDefaults {
            enabled: true,
            store: Store::Memory,
            max_memory: default_max_memory(),
            max_body_size: default_max_body(),
            respect_origin: true,
            eviction: Eviction::default(),
            tag_header: default_tag_header(),
            purge: Purge::default(),
        }
    }
}

fn yes() -> bool {
    true
}
fn default_max_memory() -> Bytes {
    Bytes(512 * 1024 * 1024)
}
fn default_max_body() -> Bytes {
    Bytes(4 * 1024 * 1024)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    #[default]
    Memory,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoalesceDefaults {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// How long a waiter will wait on the in-flight leader.
    ///
    /// Defaults to `None`, meaning "as long as the origin request itself may
    /// take" — resolved from `timeouts.origin` at load. A shorter value than
    /// the origin timeout releases every waiter *before* the work they are
    /// waiting on can possibly finish, which manufactures the stampede this
    /// exists to prevent.
    #[serde(default)]
    pub wait_timeout: Option<Dur>,
    /// What a waiter does when its wait expires. Independent execution is off
    /// by default for the reason above.
    #[serde(default)]
    pub on_timeout: OnCoalesceTimeout,
}

impl Default for CoalesceDefaults {
    fn default() -> Self {
        CoalesceDefaults {
            enabled: true,
            wait_timeout: None,
            on_timeout: OnCoalesceTimeout::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnCoalesceTimeout {
    /// Serve stale if a permitted entry exists, otherwise shed.
    #[default]
    StaleOrShed,
    /// Re-enter admission individually. Rate-limited by the admission
    /// controller, never released as a group.
    Requeue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    #[serde(default = "d_500ms")]
    pub connect: Dur,
    #[serde(default = "d_5s")]
    pub first_byte: Dur,
    #[serde(default = "d_30s")]
    pub idle: Dur,
    /// Ceiling on one origin request end to end. Also the floor for
    /// `coalesce.wait_timeout`.
    #[serde(default = "d_30s")]
    pub origin: Dur,
    /// How long Tanod will wait on a stalled *client* before abandoning the
    /// downstream write. Without this a slow reader occupies a slot forever.
    #[serde(default = "d_30s")]
    pub downstream_write: Dur,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: d_500ms(),
            first_byte: d_5s(),
            idle: d_30s(),
            origin: d_30s(),
            downstream_write: d_30s(),
        }
    }
}

fn d_500ms() -> Dur {
    Dur(std::time::Duration::from_millis(500))
}
fn d_5s() -> Dur {
    Dur(std::time::Duration::from_secs(5))
}
fn d_10s() -> Dur {
    Dur(std::time::Duration::from_secs(10))
}
fn d_30s() -> Dur {
    Dur(std::time::Duration::from_secs(30))
}

/// A bounded buffer between the origin and a slow client.
///
/// Without it, an origin work permit is held until the *client* has finished
/// reading, because Pingora paces upstream reads against downstream writes.
/// With it, response body bytes are absorbed here so the origin is never made
/// to wait on the client, `end_of_stream` is observed when the origin has
/// actually finished, and the permit goes back then.
///
/// The cost is progressive rendering: a spooled response reaches the client
/// only once the origin has finished producing it. That is why it is off by
/// default and set per route.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spool {
    /// Default for every route. A route may override it either way.
    #[serde(default)]
    pub enabled: bool,
    /// Ceiling on one response. A body that outgrows it stops being spooled:
    /// what is buffered is flushed, the rest streams through, and the permit
    /// is held as it was before — bounded by `timeouts.downstream_write`.
    #[serde(default = "default_spool_body")]
    pub max_body: Bytes,
    /// Ceiling across every in-flight spool at once. This is the number that
    /// stops a thousand slow readers from turning a per-request bound into a
    /// process-wide one.
    #[serde(default = "default_spool_memory")]
    pub max_memory: Bytes,
}

impl Default for Spool {
    fn default() -> Self {
        Spool {
            enabled: false,
            max_body: default_spool_body(),
            max_memory: default_spool_memory(),
        }
    }
}

fn default_spool_body() -> Bytes {
    Bytes(2 * 1024 * 1024)
}
fn default_spool_memory() -> Bytes {
    Bytes(256 * 1024 * 1024)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpool {
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `Upgrade`-carrying requests: WebSocket, and anything else that turns one
/// request into a long-lived tunnel.
///
/// These are refused by default. An upgraded connection is neither cacheable
/// nor coalescible and lives far longer than a render, so admitting one
/// against the render ceiling would let a handful of sockets consume the
/// capacity the origin needs to answer pages.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upgrade {
    #[serde(default)]
    pub enabled: bool,
    /// Ceiling on concurrent upgraded connections, counted separately from
    /// `origin.concurrency`. There is no queue: a tunnel that has to wait is
    /// a tunnel that has already failed.
    #[serde(default = "default_upgrade_max")]
    pub max_concurrent: usize,
}

impl Default for Upgrade {
    fn default() -> Self {
        Upgrade {
            enabled: false,
            max_concurrent: default_upgrade_max(),
        }
    }
}

fn default_upgrade_max() -> usize {
    100
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overload {
    #[serde(default = "status_503")]
    pub status: u16,
    #[serde(default = "d_1s")]
    pub retry_after: Dur,
}

impl Default for Overload {
    fn default() -> Self {
        Overload {
            status: status_503(),
            retry_after: d_1s(),
        }
    }
}

fn status_503() -> u16 {
    503
}
fn d_1s() -> Dur {
    Dur(std::time::Duration::from_secs(1))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    /// Static build identifier, mixed into every cache key.
    #[serde(default)]
    pub id: Option<String>,
    /// Or read it from a response header the origin sets.
    #[serde(default)]
    pub id_header: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: Matcher,
    #[serde(default)]
    pub class: Option<ClassOverride>,
    /// Which share of the origin ceiling this route's requests compete for.
    /// See [`Priorities`].
    #[serde(default)]
    pub priority: Priority,
    /// How many permits one request on this route consumes, everywhere it is
    /// counted: the tier ceiling, the global ceiling, and this route's own.
    ///
    /// The default of 1 says every request costs the same, which is the
    /// assumption a plain concurrency limit makes and is wrong in the usual
    /// direction: a search page that fans out to three services and a
    /// statically-shaped marketing page are not the same load on an origin,
    /// yet a ceiling of 50 lets 50 of either through. Giving the expensive
    /// route a weight makes the ceiling mean units of origin work rather than
    /// units of request.
    #[serde(default = "one_u32")]
    pub weight: u32,
    #[serde(default)]
    pub cache: Option<RouteCache>,
    #[serde(default)]
    pub coalesce: Option<RouteCoalesce>,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub spool: Option<RouteSpool>,
}

/// `match: "/products/**"` and the expanded table form both parse.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Matcher {
    Path(String),
    Detailed(DetailedMatcher),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailedMatcher {
    #[serde(default)]
    pub host: Option<String>,
    pub path: String,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassOverride {
    Static,
    PublicSsr,
    PublicDynamic,
    PrivateDynamic,
    Streaming,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCache {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub ttl: Option<Ttl>,
    #[serde(default)]
    pub stale_while_revalidate: Option<Dur>,
    #[serde(default)]
    pub stale_if_error: Option<Dur>,
    #[serde(default)]
    pub query: Option<QueryPolicy>,
    #[serde(default)]
    pub vary: Option<VaryPolicy>,
    /// Store this route's responses even when the origin says not to.
    ///
    /// This exists because a dynamically-rendered Next.js route answers with
    /// `Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate`
    /// — so without an override, the microcache never engages on precisely the
    /// routes worth protecting. It is per-route, never global, and validation
    /// refuses to combine it with a private class or a cookie-bearing route.
    #[serde(default)]
    pub override_origin: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ttl {
    /// Ceiling applied to whatever the origin asked for. Shrinks, never grows.
    #[serde(default)]
    pub max: Option<Dur>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPolicy {
    pub mode: QueryMode,
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Only the listed keys enter the cache key. Unlisted keys are dropped,
    /// which is what stops `?cachebust=<random>` from minting a fresh key —
    /// and therefore a fresh render — on every request.
    Include,
    /// Everything except the listed keys enters the key.
    Exclude,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaryPolicy {
    #[serde(default)]
    pub headers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCoalesce {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Collapse concurrent equivalent requests even when the origin says
    /// `no-store`. Weaker than `cache.override_origin`: nothing is persisted,
    /// the window is one render, and every waiter would otherwise have
    /// rendered against the same origin state anyway.
    #[serde(default)]
    pub override_origin: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    #[serde(default)]
    pub prometheus: Option<Prometheus>,
    /// Readiness and status endpoints. Off unless configured, and never on
    /// the traffic listener — see [`Admin`].
    #[serde(default)]
    pub admin: Option<Admin>,
    /// Distributed tracing. Correlation ids are always produced; this block
    /// only decides whether spans are exported anywhere.
    #[serde(default)]
    pub tracing: Tracing,
    #[serde(default)]
    pub logging: Logging,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prometheus {
    pub listen: String,
}

/// The operator-facing listener: readiness, liveness and status.
///
/// It gets its own address rather than a path on the traffic listener for two
/// reasons. A path would collide with the origin's URL space — `/status` is a
/// perfectly ordinary application route — and it would publish the origin's
/// backend states, cache occupancy and config generation to anyone who can
/// reach the site. Bind it to a loopback or private address.
///
/// Nothing it serves is parameterised. There is no path, host, query or header
/// a client can vary to change the response, which is the same rule the
/// metrics labels follow: an operator surface must not be a way to make the
/// process do unbounded work.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    pub listen: String,
    /// Report not-ready while every upstream is failing its health check.
    ///
    /// Off by default, and the default is the careful one: Tanod still
    /// serves a fully unhealthy pool (stale-if-error exists for that window),
    /// so taking every replica out of rotation because the *origin* is down
    /// converts a degraded origin into a total outage at the edge as well.
    /// Turn it on when something upstream of Tanod can route around it.
    #[serde(default)]
    pub require_healthy_upstream: bool,
}

/// OpenTelemetry.
///
/// Request correlation — a trace id and span id on every request, in the
/// access log, and forwarded to the origin as `traceparent` — is unconditional
/// and costs nothing. This block is only about *export*: whether those spans
/// are also shipped to a collector.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracing {
    /// `service.name` on every exported span.
    #[serde(default)]
    pub service_name: Option<String>,
    /// Where spans go. Absent means spans are still built and correlated, but
    /// never leave the process.
    #[serde(default)]
    pub otlp: Option<Otlp>,
    #[serde(default)]
    pub sample: Sample,
    /// Whose `traceparent` Tanod will join.
    #[serde(default)]
    pub trust_incoming: TrustIncoming,
}

/// OTLP over HTTP, JSON encoding.
///
/// JSON rather than protobuf, and a hand-written client rather than a gRPC
/// stack: the OTLP/HTTP JSON encoding is specified, every collector accepts
/// it, and the alternative is adding a transitive dependency tree larger than
/// the rest of this binary to a process whose whole argument is that it is
/// small enough to audit.
///
/// The endpoint must be `http://`. Refusing `https://` is deliberate: a
/// hand-written exporter that quietly spoke cleartext to an endpoint written
/// as `https` would be the silent downgrade this project rejects everywhere
/// else. Run a collector as a local sidecar and let it do the transport.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Otlp {
    /// Full URL of the traces endpoint, e.g.
    /// `http://127.0.0.1:4318/v1/traces`.
    pub endpoint: String,
    /// Ceiling on one export attempt, connect to last byte.
    #[serde(default = "d_5s")]
    pub timeout: Dur,
    /// Spans buffered between exports. Bounded, and full means *drop*: a
    /// telemetry queue that grows under load is a memory-exhaustion bug that
    /// only fires during the incident you wanted the traces for.
    #[serde(default = "default_span_queue")]
    pub max_queue: usize,
    /// Spans per export request.
    #[serde(default = "default_span_batch")]
    pub max_batch: usize,
    /// How often a partial batch is flushed.
    #[serde(default = "d_2s")]
    pub interval: Dur,
}

fn default_span_queue() -> usize {
    2048
}
fn default_span_batch() -> usize {
    256
}
fn d_2s() -> Dur {
    Dur(std::time::Duration::from_secs(2))
}

/// Head sampling, as an integer ratio.
///
/// `one_in` rather than a fraction: `0.1` invites a float in a config file
/// that has no other float in it, and "one request in ten" is what an operator
/// actually says out loud.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    #[serde(default)]
    pub mode: SampleMode,
    /// Sample one request in this many. Only read when `mode: ratio`.
    #[serde(default = "one_usize")]
    pub one_in: usize,
}

impl Default for Sample {
    fn default() -> Self {
        Sample {
            mode: SampleMode::default(),
            one_in: one_usize(),
        }
    }
}

fn one_usize() -> usize {
    1
}
fn one_u32() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleMode {
    /// Follow the sampled flag on an inbound `traceparent` when there is one,
    /// and fall back to `ratio` when there is not. The default, because a
    /// trace that is sampled at the edge and unsampled here has a hole in it
    /// exactly where the origin governor sits.
    #[default]
    ParentOrRatio,
    /// Sample one request in `one_in`, ignoring what the caller decided.
    Ratio,
    /// Record everything.
    Always,
    /// Record nothing. Correlation ids are still produced and logged.
    Never,
}

/// Whether an inbound `traceparent` is believed.
///
/// A trace id chosen by the client is not a security hole on its own, but it
/// does let anyone on the internet write into your tracing backend under a
/// trace of their choosing, and join their requests to someone else's trace.
/// The default is `never`. Unlike `X-Forwarded-For`, a trace context has no hop
/// chain Tanod can walk to distinguish a value created by a trusted edge from
/// one the edge merely forwarded from an internet client. Opting into proxy
/// trust is safe only when that proxy strips or replaces inbound trace headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustIncoming {
    FromTrustedProxies,
    Always,
    #[default]
    Never,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
    #[serde(default)]
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}
