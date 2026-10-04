//! The Pingora proxy layer.
//!
//! Order of operations is the whole design, and it is why admission lives in
//! [`ProxyHttp::proxy_upstream_filter`] rather than `request_filter`:
//!
//! ```text
//! early_request_filter  resolve the client address and scheme from the
//!                       connection, believing forwarded headers only from a
//!                       trusted peer
//! request_filter        classify, resolve route
//! request_cache_filter  enable the cache if this route may reuse
//! cache lookup          hit, or wait on the cache lock as a coalesced follower
//! proxy_upstream_filter admission — only reached on a genuine miss
//! upstream_peer         pick a backend
//! response_body_filter  spool the body, so the origin is never paced by the
//!                       client and the permit can go back when the origin is
//!                       genuinely finished
//! ```
//!
//! Putting admission earlier would make cache hits and coalesced followers
//! queue for origin capacity they never consume. Pingora documents this hook
//! for exactly this purpose: "deferring checks like rate limiting ... to when
//! they are actually needed after cache miss".

pub mod spool;

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_cache::key::CacheKey as PingoraCacheKey;
use pingora_cache::{CacheMeta, CacheOptionOverrides, NoCacheReason, RespCacheable};
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

use crate::admission::limiter::{Limiter, Permit};
use crate::admission::{Admission, AdmissionController};
use crate::cache::policy::{Disposition, Shareability, evaluate_request, evaluate_response};
use crate::cache::{BoundedStore, KeyBuilder};
use crate::classifier::{FrameworkAdapter, RequestClass, RequestMetadata, nextjs::NextJs};
use crate::config::schema::{
    LoadBalancing, LogFormat, Mode, OriginHttpVersion, Priority, RouteCache, Timeouts,
};
use crate::net::forwarded::{ClientFacts, ListenerScheme, TrustPolicy};
use crate::policy::PolicySnapshot;
use crate::proxy::spool::{Spool, SpoolBudget, SpoolOutcome};
use crate::telemetry::logging::AccessLog;
use crate::telemetry::metrics;
use crate::telemetry::otlp::{Attr, SpanKind, SpanRecord, SpanSink, unix_nanos};
use crate::telemetry::trace::{RequestTrace, SpanId, TRACEPARENT, TRACESTATE, believe_inbound};
use crate::upstream::breaker::BreakerState;
use crate::upstream::retry::{self, RetryBudget, RetryDecision};
use crate::upstream::{InFlightGuard, UpstreamPool};

/// Per-request state.
pub struct Ctx {
    /// The policy generation this request started on. Pinned here so a reload
    /// mid-request cannot apply half of itself to one request.
    pub policy: Arc<PolicySnapshot>,
    pub started: Instant,
    pub class: RequestClass,
    pub route_id: Option<String>,
    /// Origin capacity, held for as long as this request is consuming it.
    ///
    /// Released at the upstream's end of stream. Whether that instant is
    /// honest depends on the spool: with one, the origin was never paced by
    /// the client, so end of stream *is* the moment the origin finished.
    /// Without one, Pingora pairs upstream reads with downstream writes and a
    /// slow reader delays the observation — bounded only by
    /// `timeouts.downstream_write`. See [`spool`].
    pub permit: Option<Permit>,
    /// Capacity for an upgraded connection, counted separately from renders
    /// because it is held for the life of a tunnel rather than a render.
    pub upgrade_permit: Option<Permit>,
    pub shed: bool,
    pub upstream: Option<String>,
    pub origin_started: Option<Instant>,
    pub origin_finished_ms: Option<u128>,

    /// Which share of the origin ceiling this request competes for, and how
    /// many units of it one request costs. Resolved from the route in
    /// `request_filter` and read back in `proxy_upstream_filter`.
    pub priority: Priority,
    pub weight: u32,

    /// This request's occupancy of a backend, released when the origin
    /// finishes. Replacing it on a retry releases the previous backend's slot,
    /// which is what keeps a retried request from being counted on two
    /// backends at once.
    pub origin_slot: Option<InFlightGuard>,
    /// How many times this request has been sent upstream, the first included.
    pub attempts: u32,
    /// Whether this attempt has already told the breaker how it went. Exactly
    /// one outcome is recorded per attempt, decided by the response header or
    /// by whatever prevented one.
    pub outcome_recorded: bool,

    /// The connection facts, resolved once in `early_request_filter`.
    ///
    /// Resolved there rather than read at each use so that one request cannot
    /// see two different answers, and so the trusted-proxy decision is made
    /// exactly once per request.
    pub client: ClientFacts,

    /// Cache policy resolved during `request_filter` and consumed later in the
    /// pipeline, where the request header is no longer convenient to re-read.
    pub route_cache: Option<RouteCache>,
    pub key_headers: Vec<String>,
    pub transient_only: bool,
    pub cache_active: bool,
    pub may_coalesce: bool,
    pub coalesce_override: bool,
    /// Where the origin permit was given back, for the access log.
    pub permit_released_at: Option<&'static str>,

    /// Does this route ask for a response spool?
    pub spool_enabled: bool,
    /// The buffer itself, created once the request holds a permit worth
    /// protecting. `None` means the body streams straight through.
    pub spool: Option<Spool>,
    /// Recorded before the spool is dropped, for the access log.
    pub spool_outcome: Option<SpoolOutcome>,

    /// Correlation for this request: trace id, this request's span, and the
    /// child span the origin fetch runs under. Present on every request
    /// whether or not anything is exporting spans — the ids are what join
    /// Tanod's access log to the origin's own.
    pub trace: RequestTrace,
    /// Wall clock at request start, kept only so a span can carry a real
    /// timestamp. `started` is a monotonic `Instant` and cannot be turned
    /// into one after the fact.
    pub wall_started: std::time::SystemTime,
    /// The same, for the origin fetch span.
    pub origin_wall_started: Option<std::time::SystemTime>,
}

impl Ctx {
    fn new(policy: Arc<PolicySnapshot>) -> Self {
        let sample = policy.config.telemetry.tracing.sample.clone();
        Ctx {
            policy,
            started: Instant::now(),
            class: RequestClass::Unknown,
            route_id: None,
            permit: None,
            upgrade_permit: None,
            shed: false,
            upstream: None,
            origin_started: None,
            origin_finished_ms: None,
            priority: Priority::default(),
            weight: 1,
            origin_slot: None,
            attempts: 0,
            outcome_recorded: false,
            client: ClientFacts {
                client_ip: None,
                scheme: "http",
                peer_trusted: false,
            },
            route_cache: None,
            key_headers: Vec::new(),
            transient_only: false,
            cache_active: false,
            may_coalesce: false,
            coalesce_override: false,
            permit_released_at: None,
            spool_enabled: false,
            spool: None,
            spool_outcome: None,
            // Replaced in `early_request_filter` once the peer's trust status
            // is known. Built here rather than left as an `Option` so that
            // every path out of the proxy — including the ones that never
            // reach a filter — still has a correlation id to log.
            trace: RequestTrace::begin(None, None, &sample),
            wall_started: std::time::SystemTime::now(),
            origin_wall_started: None,
        }
    }
}

pub struct Tanod {
    policy: Arc<ArcSwap<PolicySnapshot>>,
    admission: Arc<AdmissionController>,
    upstreams: Arc<UpstreamPool>,
    adapter: Arc<dyn FrameworkAdapter>,
    store: &'static BoundedStore,
    /// Built once at startup. Pingora's lock constructor controls how long a
    /// writer may own a key; follower wait time is configured per request.
    cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl,
    /// Who may describe the client. Compiled once; a reload that changes it is
    /// refused for the same reason a listen-address change is, because a
    /// request already in flight would otherwise straddle two trust models.
    trust: TrustPolicy,
    /// The process-wide ceiling on spooled response bytes.
    spool_budget: Arc<SpoolBudget>,
    /// Concurrent upgraded connections. Separate from the render ceiling: a
    /// tunnel is held for minutes, a render for milliseconds, and letting the
    /// two share a budget means a handful of sockets can starve every page.
    upgrades: Arc<Limiter>,
    /// Where finished spans go, when export is configured. `None` means spans
    /// are never built — correlation ids still are, because they cost nothing
    /// and the access log carries them regardless.
    spans: Option<SpanSink>,
    /// The origin-wide retry budget. One per process, because the thing it
    /// protects — the origin — is also one per process.
    retry: Arc<RetryBudget>,
}

impl Tanod {
    pub fn new(
        policy: Arc<ArcSwap<PolicySnapshot>>,
        admission: Arc<AdmissionController>,
        upstreams: Arc<UpstreamPool>,
        spans: Option<SpanSink>,
    ) -> std::result::Result<Self, String> {
        let initial = policy.load();
        // `Storage` takes `&'static self` throughout, so the store and the
        // lock are created once and leaked deliberately at startup.
        let store = BoundedStore::new(&initial.config.cache);
        let cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl = Box::leak(
            pingora_cache::lock::CacheLock::new_boxed(initial.config.timeouts.origin.as_duration()),
        );
        let trust = TrustPolicy::build(&initial.config.server.trusted_proxies)
            .map_err(|error| format!("server.trusted_proxies: {error}"))?;
        let spool_budget = SpoolBudget::new(initial.config.spool.max_memory.as_usize());
        // No queue: a tunnel that has to wait for a slot is a tunnel whose
        // handshake has already timed out somewhere else.
        let upgrades = Limiter::new(
            "upgrade",
            initial.config.upgrade.max_concurrent,
            0,
            std::time::Duration::ZERO,
        );
        // Bound once at startup like the trust policy and the cache lock. A
        // reload that changed the budget while retries were in flight would
        // have two windows disagreeing about how much has been spent, so a
        // change is refused rather than half-applied.
        let retry = Arc::new(RetryBudget::new(&initial.config.origin.retry));
        drop(initial);

        Ok(Tanod {
            store,
            cache_lock,
            adapter: Arc::new(NextJs),
            upstreams,
            admission,
            policy,
            trust,
            spool_budget,
            upgrades,
            spans,
            retry,
        })
    }

    /// The response cache, for the admin status document. Shared, not copied:
    /// occupancy read from a snapshot would be a number from startup.
    pub fn store(&self) -> &'static BoundedStore {
        self.store
    }

    /// The process-wide spool budget, for the same reason.
    pub fn spool_budget(&self) -> Arc<SpoolBudget> {
        self.spool_budget.clone()
    }

    /// The upgrade limiter, for the same reason.
    pub fn upgrade_limiter(&self) -> Arc<Limiter> {
        self.upgrades.clone()
    }

    /// The retry budget, for the same reason: what it has spent is live state,
    /// and a status document assembled from configuration would only ever
    /// report the ceiling.
    pub fn retry_budget(&self) -> Arc<RetryBudget> {
        self.retry.clone()
    }

    /// Build and enqueue this request's spans.
    ///
    /// Two of them when the request reached an origin: the server span for
    /// what Tanod did, and a client span for the origin fetch nested under
    /// it. The nesting is what makes an origin-latency number attributable —
    /// a single flat span cannot distinguish "the origin was slow" from "we
    /// queued for two seconds before asking it".
    ///
    /// Enqueueing is `try_send` and nothing else. This runs on the request
    /// path, so it must not block, allocate unboundedly, or fail in a way the
    /// caller has to handle.
    fn record_spans(
        &self,
        sink: &SpanSink,
        session: &Session,
        ctx: &Ctx,
        access: &AccessLog<'_>,
        status: u16,
    ) {
        let ended = std::time::SystemTime::now();
        let route = access.route;
        // Low cardinality by construction: a validated method and a route id
        // from the config file. Never the path — that is an attribute, where
        // an unbounded value costs storage rather than breaking grouping.
        let name = format!("{} {route}", access.method);
        let error = status >= 500 || ctx.shed;

        if let (Some(origin_span), Some(origin_started)) =
            (ctx.trace.origin_span_id, ctx.origin_wall_started)
        {
            let origin_ended = origin_started
                .checked_add(std::time::Duration::from_millis(
                    u64::try_from(access.origin_ms).unwrap_or(u64::MAX),
                ))
                .unwrap_or(ended);
            sink.record(SpanRecord {
                trace_id: ctx.trace.trace_id,
                span_id: origin_span,
                parent_span_id: Some(ctx.trace.span_id),
                name: format!("{name} origin"),
                kind: SpanKind::Client,
                start_unix_nano: unix_nanos(origin_started),
                end_unix_nano: unix_nanos(origin_ended),
                error,
                attributes: vec![
                    Attr::str("http.request.method", access.method),
                    Attr::str("server.address", access.upstream.unwrap_or("-")),
                    Attr::str("tanod.route", route),
                    Attr::int("http.response.status_code", i64::from(status)),
                ],
            });
        }

        sink.record(SpanRecord {
            trace_id: ctx.trace.trace_id,
            span_id: ctx.trace.span_id,
            parent_span_id: ctx.trace.parent_span_id,
            name,
            kind: SpanKind::Server,
            start_unix_nano: unix_nanos(ctx.wall_started),
            end_unix_nano: unix_nanos(ended),
            error,
            attributes: vec![
                Attr::str("http.request.method", access.method),
                // Path, never the query string — the same rule the access log
                // follows, and for the same reason: a query string routinely
                // carries session tokens and signed URLs.
                Attr::str("url.path", access.path),
                Attr::str("url.scheme", access.scheme),
                Attr::str("server.address", request_host(session.req_header())),
                Attr::str("client.address", access.client),
                Attr::int("http.response.status_code", i64::from(status)),
                Attr::str("tanod.route", route),
                Attr::str("tanod.class", access.class),
                Attr::str("tanod.cache", access.cache),
                Attr::bool("tanod.shed", ctx.shed),
                Attr::str("tanod.permit_released", access.permit_released_at),
                Attr::str("tanod.spool", access.spool),
                Attr::int(
                    "tanod.config_generation",
                    i64::try_from(ctx.policy.generation).unwrap_or(i64::MAX),
                ),
            ],
        });
    }

    /// Tell the backend's breaker how one origin attempt went.
    ///
    /// Exactly one outcome per attempt, decided by the first definitive thing
    /// that happens: the response header, or whatever prevented one. A body
    /// error after a successful header is therefore counted as the success it
    /// began as — by then the origin has demonstrably produced a response, and
    /// what went wrong afterwards is as likely to be the client as the origin.
    fn record_origin_outcome(&self, ctx: &mut Ctx, ok: bool, kind: &'static str) {
        if ctx.outcome_recorded {
            return;
        }
        let Some((id, probe)) = ctx
            .origin_slot
            .as_ref()
            .map(|slot| (slot.backend_id(), slot.probe_token()))
        else {
            return;
        };
        ctx.outcome_recorded = true;

        let was = self.upstreams.breaker_state(id);
        self.upstreams.record_outcome(id, probe, ok);
        let now = self.upstreams.breaker_state(id);

        let Some(address) = ctx.upstream.as_deref() else {
            return;
        };
        if !ok {
            metrics::UPSTREAM_FAILURES
                .with_label_values(&[address, kind])
                .inc();
        }
        if was == BreakerState::Closed && now == BreakerState::Open {
            metrics::UPSTREAM_TRIPS.with_label_values(&[address]).inc();
            log::warn!("upstream {address} ejected: too many recent failures");
        }
        if was == BreakerState::Open && now == BreakerState::Closed {
            log::info!("upstream {address} back in rotation: recovery probe succeeded");
        }
        metrics::UPSTREAM_EJECTED
            .with_label_values(&[address])
            .set(i64::from(now == BreakerState::Open));
    }

    /// Publish the one tier this request can have changed. The other tiers are
    /// independent, so scanning and touching all three on every request only
    /// adds metric-registry work without making their values fresher.
    fn publish_tier_state(&self, priority: Priority) {
        let Some(tier) = self.admission.tier_limiter(priority) else {
            return;
        };
        metrics::LIMIT
            .with_label_values(&[tier.name()])
            .set(tier.limit() as i64);
        metrics::QUEUE_DEPTH
            .with_label_values(&[tier.name()])
            .set(tier.queue_depth() as i64);
        metrics::IN_FLIGHT
            .with_label_values(&[tier.name()])
            .set((tier.limit().saturating_sub(tier.available())) as i64);
    }

    /// Publish only the backend whose state this request touched.
    fn publish_backend_state(&self, id: usize, address: &str) {
        metrics::UPSTREAM_IN_FLIGHT
            .with_label_values(&[address])
            .set(i64::try_from(self.upstreams.in_flight(id)).unwrap_or(i64::MAX));
        metrics::UPSTREAM_LATENCY_EWMA
            .with_label_values(&[address])
            .set(i64::try_from(self.upstreams.ewma_micros(id)).unwrap_or(i64::MAX));
        metrics::UPSTREAM_EJECTED
            .with_label_values(&[address])
            .set(i64::from(
                self.upstreams.breaker_state(id) == BreakerState::Open,
            ));
    }

    /// Release and publish one attempt's backend occupancy. This is also used
    /// before a retry replaces the attempt, so both the old and new backend
    /// gauges remain exact without an O(backends) completion scan.
    fn release_origin_slot(&self, ctx: &mut Ctx) {
        let Some(slot) = ctx.origin_slot.take() else {
            return;
        };
        let id = slot.backend_id();
        let address = ctx.upstream.clone();
        drop(slot);
        if let Some(address) = address {
            self.publish_backend_state(id, &address);
        }
    }

    /// May this request be sent upstream again, and is there budget for it?
    ///
    /// Called from both error hooks, and it charges the budget, so it runs
    /// once per failed attempt and not once per question asked about one.
    fn decide_retry(&self, session: &Session, ctx: &mut Ctx) -> RetryDecision {
        if !self.retry.enabled() {
            return RetryDecision::Ineligible;
        }
        let decision = if retry::eligible(&session.req_header().method, ctx.class) {
            self.retry.charge(self.upstreams.now_ms(), ctx.attempts)
        } else {
            RetryDecision::Ineligible
        };
        metrics::RETRIES
            .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), decision.as_str()])
            .inc();
        metrics::RETRY_BUDGET
            .set(i64::try_from(self.retry.allowance(self.upstreams.now_ms())).unwrap_or(i64::MAX));
        decision
    }

    /// Route limiter for this request, created on first use.
    fn limiter_for(&self, policy: &PolicySnapshot, route_id: &str) -> Option<Arc<Limiter>> {
        let route = policy.routes.iter().find(|r| r.id == route_id)?;
        let c = route.config.concurrency.as_ref()?;
        Some(self.admission.route_limiter(
            route_id,
            c.max,
            c.queue.max,
            c.queue.timeout.as_duration(),
        ))
    }

    /// Answer an upgrade request that Tanod will not proxy.
    ///
    /// `501` rather than the overload status: nothing is overloaded, the proxy
    /// simply does not implement this. Sending the overload `503` would invite
    /// a client to retry something that will never succeed, and would put a
    /// configuration mistake into the same metric as real origin pressure.
    async fn refuse_upgrade(&self, session: &mut Session, policy: &PolicySnapshot) -> Result<()> {
        let mut resp = ResponseHeader::build(501, Some(3))?;
        resp.insert_header("Cache-Control", "no-store")?;
        resp.insert_header("Connection", "close")?;
        if policy.config.debug_headers {
            resp.insert_header("X-Tanod", "UPGRADE-DISABLED")?;
        }
        session.as_downstream_mut().set_keepalive(None);
        session.write_response_header(Box::new(resp), true).await?;
        Ok(())
    }

    async fn refuse(&self, session: &mut Session, policy: &PolicySnapshot) -> Result<()> {
        let overload = &policy.config.overload;
        let mut resp = ResponseHeader::build(overload.status, Some(3))?;
        let retry_after = overload.retry_after.as_duration().as_secs().max(1);
        resp.insert_header("Retry-After", retry_after.to_string())?;
        // A CDN that caches this turns a brief origin blip into a long outage.
        resp.insert_header("Cache-Control", "no-store")?;
        if policy.config.debug_headers {
            resp.insert_header("X-Tanod", "SHED")?;
        }
        // proxy_upstream_filter(false) is reusable by default in Pingora. The
        // request body may still be unread, so keeping this connection alive
        // could make those bytes look like the next request.
        session.as_downstream_mut().set_keepalive(None);
        session.write_response_header(Box::new(resp), true).await?;
        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for Tanod {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        // Pin the generation for the whole request.
        Ctx::new(self.policy.load_full())
    }

    async fn early_request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<()> {
        // Bound how long a single downstream write may block.
        //
        // This is what keeps a deliberately slow reader from occupying an
        // origin permit indefinitely on a response too large to fit in the
        // buffers between origin and client. It does not eliminate the
        // coupling — see the slow-reader note in the README — but it turns an
        // unbounded hold into a bounded one.
        let timeout = ctx.policy.config.timeouts.downstream_write.as_duration();
        if !timeout.is_zero() {
            session.as_downstream_mut().set_write_timeout(Some(timeout));
        }

        // Resolve who the client is and what scheme they used, once, before
        // anything reads either. Both are claims when Tanod sits behind a
        // load balancer, and the scheme in particular is part of the cache
        // key — a client that could set it would own a key dimension and turn
        // one URL into an unbounded number of renders.
        ctx.client = self.trust.resolve(
            session
                .client_addr()
                .and_then(|address| address.as_inet())
                .map(|address| address.ip()),
            &session.req_header().headers,
            listener_scheme(session),
        );

        // An inbound `traceparent` is a claim, exactly like `X-Forwarded-For`,
        // so it is gated on the same trust decision — which is why this runs
        // *after* `trust.resolve` and not before. Believing an untrusted one
        // would let anyone on the internet write into the tracing backend
        // under a trace of their choosing.
        let tracing = &ctx.policy.config.telemetry.tracing;
        let believe = believe_inbound(tracing.trust_incoming, ctx.client.peer_trusted);
        let headers = &session.req_header().headers;
        ctx.trace = RequestTrace::begin(
            believe.then(|| header_str(headers, TRACEPARENT)).flatten(),
            believe.then(|| header_str(headers, TRACESTATE)).flatten(),
            &tracing.sample,
        );
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let req = session.req_header();
        let host = request_host(req);
        let path = req.uri.path().to_string();
        let query = req.uri.query().map(str::to_string);
        let method = req.method.clone();

        let meta = RequestMetadata {
            method: &method,
            host: &host,
            path: &path,
            query: query.as_deref(),
            headers: &req.headers,
        };

        let hints = self.adapter.classify_request(&meta);
        let policy = ctx.policy.clone();
        let route = policy.resolve(&host, &path, &method);

        // A route's declared class normally outranks what the classifier
        // inferred: the operator knows things about their own routes that
        // headers do not reveal. Protocol upgrades are the exception. They
        // leave HTTP behind, so no route label may turn one back into a
        // cacheable, render-limited, or admission-exempt HTTP request.
        ctx.class =
            resolved_request_class(&meta, route.and_then(|r| r.declared_class()), hints.class);
        ctx.route_id = route.map(|r| r.id.clone());
        // Resolved here, where the route is in hand, and read back in
        // `proxy_upstream_filter`, which runs after the cache has had its say
        // and no longer has it.
        ctx.priority = route.map_or(Priority::Normal, |r| r.config.priority);
        ctx.weight = route.map_or(1, |r| r.config.weight);

        ctx.route_cache = route.and_then(|r| r.config.cache.clone());
        ctx.key_headers = resolved_key_headers(&hints.key_headers, ctx.route_cache.as_ref());
        let coalesce_override = route
            .and_then(|r| r.config.coalesce.as_ref())
            .is_some_and(|c| c.override_origin);
        ctx.coalesce_override = coalesce_override;
        let protecting = ctx.policy.config.mode == Mode::Protect;
        let route_cache_enabled = protecting
            && ctx.policy.config.cache.enabled
            && ctx
                .route_cache
                .as_ref()
                .and_then(|cache| cache.enabled)
                .unwrap_or(true);
        let route_coalesce_enabled = protecting
            && route
                .and_then(|r| r.config.coalesce.as_ref())
                .and_then(|coalesce| coalesce.enabled)
                .unwrap_or(ctx.policy.config.coalesce.enabled);

        let route_label = ctx.route_id.as_deref().unwrap_or("-");
        metrics::REQUESTS
            .with_label_values(&[route_label, ctx.class.as_str()])
            .inc();

        // Spooling is per route, defaulting to the global setting. Decided
        // here because this is the last hook that has the route in hand and
        // the first that runs before any response byte exists.
        ctx.spool_enabled = route
            .and_then(|r| r.config.spool.as_ref())
            .and_then(|spool| spool.enabled)
            .unwrap_or(ctx.policy.config.spool.enabled);
        if !protecting {
            ctx.spool_enabled = false;
        }

        // An upgrade leaves HTTP behind, so every filter after this one stops
        // applying. Refuse it here, before the cache is consulted and before
        // a backend is chosen.
        if ctx.class == RequestClass::Upgrade && !ctx.policy.config.upgrade.enabled {
            metrics::UPGRADES
                .with_label_values(&[route_label, "disabled"])
                .inc();
            let policy = ctx.policy.clone();
            self.refuse_upgrade(session, &policy).await?;
            return Ok(true);
        }

        let disposition = evaluate_request(
            &meta,
            ctx.class,
            hints.force_bypass,
            ctx.route_cache.as_ref(),
            coalesce_override,
            route_cache_enabled,
            route_coalesce_enabled,
        );
        if let Disposition::Eligible {
            mut may_store,
            may_coalesce,
        } = disposition
        {
            if hints.coalesce_only {
                may_store = false;
            }
            ctx.cache_active = may_store || may_coalesce;
            ctx.transient_only = !may_store;
            ctx.may_coalesce = may_coalesce;
        }

        // The denominator of the origin-work-avoidance ratio, counted only
        // where reuse was genuinely possible.
        if ctx.cache_active {
            metrics::REUSE_ELIGIBLE
                .with_label_values(&[route_label])
                .inc();
        }

        Ok(false)
    }

    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<()> {
        if !ctx.cache_active {
            return Ok(());
        }
        let lock_options = ctx.may_coalesce.then(|| {
            let mut options = CacheOptionOverrides::default();
            options.wait_timeout = Some(ctx.policy.coalesce_wait());
            options
        });
        session.cache.enable(
            self.store,
            None,
            None,
            ctx.may_coalesce.then_some(self.cache_lock),
            lock_options,
        );
        // Upstream tracks body bytes and marks the response uncacheable past
        // this limit, so an oversized body streams to the client without
        // being retained.
        session
            .cache
            .set_max_file_size_bytes(ctx.policy.config.cache.max_body_size.as_usize());
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session, ctx: &mut Ctx) -> Result<PingoraCacheKey> {
        let req = session.req_header();
        let host = request_host(req);
        let path = req.uri.path().to_string();
        let query = req.uri.query().map(str::to_string);

        let meta = RequestMetadata {
            method: &req.method,
            host: &host,
            path: &path,
            query: query.as_deref(),
            headers: &req.headers,
        };

        let key = KeyBuilder {
            // The effective scheme, not the listener's. An https request and
            // an http one for the same URL are different entries: they can
            // legitimately produce different bodies (absolute URLs, redirects,
            // HSTS), and merging them is how a plaintext response reaches a
            // client that asked for TLS.
            scheme: ctx.client.scheme,
            query_policy: ctx.route_cache.as_ref().and_then(|c| c.query.as_ref()),
            variant_headers: &ctx.key_headers,
            deployment: ctx.policy.config.deployment.id.as_deref(),
        }
        .build(&meta);

        // The path rides along in `user_tag`, which Pingora hashes into
        // *nothing*: `primary_hasher` covers only the namespace and the
        // primary key. Carrying it here is what makes `POST /purge?path=…`
        // possible at all — entries are keyed by a hash, so without the path
        // stored somewhere there is no way back from a URL to its entries —
        // and doing it in the one field that cannot change the key means it
        // cannot change what is shared with whom. `cache::key` remains the
        // sole authority on that, which the test below pins.
        Ok(PingoraCacheKey::new(
            "",
            key.canonical_string(),
            purgeable_path(&path),
        ))
    }

    /// Admission. Reached only on a genuine cache miss, so hits and coalesced
    /// followers never consume origin capacity.
    async fn proxy_upstream_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        if matches!(
            session.cache.phase(),
            pingora_cache::CachePhase::Disabled(NoCacheReason::CacheLockTimeout)
        ) && ctx.policy.config.coalesce.on_timeout
            == crate::config::schema::OnCoalesceTimeout::StaleOrShed
        {
            ctx.shed = true;
            self.refuse(session, &ctx.policy).await?;
            return Ok(false);
        }
        let route_label = ctx.route_id.as_deref().unwrap_or("-").to_string();

        // An upgrade is admitted against its own ceiling and never against
        // the render ceiling. `admit` would return `Exempt` for this class,
        // which would let an unbounded number of tunnels through.
        if ctx.class == RequestClass::Upgrade {
            // No queue: `acquire` with no deadline either takes a slot now or
            // refuses now.
            match self.upgrades.acquire(None, 1).await {
                Ok(permit) => {
                    ctx.upgrade_permit = Some(permit);
                    metrics::UPGRADES
                        .with_label_values(&[&route_label, "admitted"])
                        .inc();
                    metrics::UPGRADES_ACTIVE.set(
                        (self
                            .upgrades
                            .limit()
                            .saturating_sub(self.upgrades.available()))
                            as i64,
                    );
                    return Ok(true);
                }
                Err(reason) => {
                    ctx.shed = true;
                    metrics::UPGRADES
                        .with_label_values(&[&route_label, &format!("shed_{}", reason.as_str())])
                        .inc();
                    let policy = ctx.policy.clone();
                    self.refuse(session, &policy).await?;
                    return Ok(false);
                }
            }
        }

        if ctx.policy.config.mode == Mode::Observe {
            metrics::ADMISSION
                .with_label_values(&[&route_label, "observe"])
                .inc();
            return Ok(true);
        }

        let route_limiter = ctx
            .route_id
            .as_deref()
            .and_then(|id| self.limiter_for(&ctx.policy, id));
        let outcome = self
            .admission
            .admit(ctx.class, route_limiter.as_ref(), ctx.priority, ctx.weight)
            .await;

        // Publish limiter state on the way through rather than from a timer:
        // these are the numbers an operator wants during an incident, and a
        // sampled gauge would miss the spike that caused it.
        let global = self.admission.global();
        metrics::LIMIT
            .with_label_values(&["global"])
            .set(global.limit() as i64);
        metrics::QUEUE_DEPTH
            .with_label_values(&["global"])
            .set(global.queue_depth() as i64);
        metrics::IN_FLIGHT
            .with_label_values(&["global"])
            .set((global.limit().saturating_sub(global.available())) as i64);
        if let Some(l) = &route_limiter {
            metrics::LIMIT
                .with_label_values(&[l.name()])
                .set(l.limit() as i64);
            metrics::QUEUE_DEPTH
                .with_label_values(&[l.name()])
                .set(l.queue_depth() as i64);
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
        }
        self.publish_tier_state(ctx.priority);

        match outcome {
            Admission::Admitted(permits) => {
                ctx.permit = Some(permits.into_inner());
                // The spool exists to give a permit back early, so it is
                // created only where there is a permit to give back. A class
                // that is exempt from admission — static, streaming — has
                // nothing to gain and everything to lose from being buffered.
                if ctx.spool_enabled {
                    ctx.spool = Some(Spool::new(
                        self.spool_budget.clone(),
                        ctx.policy.config.spool.max_body.as_usize(),
                    ));
                }
                metrics::ADMISSION
                    .with_label_values(&[&route_label, "admitted"])
                    .inc();
                Ok(true)
            }
            Admission::Exempt => {
                metrics::ADMISSION
                    .with_label_values(&[&route_label, "exempt"])
                    .inc();
                Ok(true)
            }
            Admission::Shed(reason) => {
                ctx.shed = true;
                metrics::ADMISSION
                    .with_label_values(&[&route_label, &format!("shed_{}", reason.as_str())])
                    .inc();
                self.refuse(session, &ctx.policy).await?;
                Ok(false)
            }
        }
    }

    /// Whether a stale entry may be served.
    ///
    /// Pingora only calls this once it has already confirmed the entry is
    /// inside its stale window, so the window itself comes from `CacheMeta` —
    /// which is to say from the route's `stale_while_revalidate` /
    /// `stale_if_error`, falling back to whatever the origin asked for. This
    /// is purely the policy question of whether to use it.
    ///
    /// The default implementation answers `false` when `error` is `None`,
    /// which is precisely the stale-while-revalidate path — so without this
    /// override, configuring `stale_while_revalidate` would quietly do
    /// nothing.
    fn should_serve_stale(
        &self,
        _session: &mut Session,
        _ctx: &mut Ctx,
        error: Option<&Error>,
    ) -> bool {
        match error {
            // Background revalidation in flight: serve the stale copy now.
            None => true,
            // Only an origin failure justifies stale. An error raised by
            // Tanod itself (a shed, a bad config) is not a reason to hand
            // out old content.
            Some(e) => e.esource() == &pingora_core::ErrorSource::Upstream,
        }
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<RespCacheable> {
        let cache_control = joined_header_values(&resp.headers, http::header::CACHE_CONTROL);
        let vary = joined_header_values(&resp.headers, http::header::VARY);
        let meta = crate::cache::policy::ResponseMetadata {
            status: resp.status.as_u16(),
            cache_control: cache_control.as_deref(),
            set_cookie: resp.headers.contains_key("set-cookie"),
            vary: vary.as_deref(),
        };

        match evaluate_response(
            &meta,
            ctx.route_cache.as_ref(),
            &ctx.key_headers,
            ctx.transient_only,
            ctx.coalesce_override,
        ) {
            Shareability::Shareable { ttl, swr, sie } => {
                let now = std::time::SystemTime::now();
                let fresh_until = now.checked_add(ttl).ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "cache TTL exceeds SystemTime range",
                    )
                })?;
                let mut stored = resp.clone();
                // Never let an origin-supplied value collide with Tanod's
                // private storage marker.
                stored.remove_header(crate::cache::TRANSIENT_HEADER);
                Ok(RespCacheable::Cacheable(CacheMeta::new(
                    fresh_until,
                    now,
                    u32::try_from(swr.as_secs()).unwrap_or(u32::MAX),
                    u32::try_from(sie.as_secs()).unwrap_or(u32::MAX),
                    stored,
                )))
            }
            // Collapse the in-flight herd onto one render, retain nothing: an
            // entry born stale is served to everyone already waiting on the
            // lock and to nobody afterwards.
            Shareability::TransientOnly => {
                let now = std::time::SystemTime::now();
                let freshness = ctx
                    .policy
                    .coalesce_wait()
                    .max(std::time::Duration::from_millis(1));
                let fresh_until = now.checked_add(freshness).ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "coalesce timeout exceeds SystemTime range",
                    )
                })?;
                let mut transient = resp.clone();
                transient.remove_header(crate::cache::TRANSIENT_HEADER);
                transient.insert_header(crate::cache::TRANSIENT_HEADER, "1")?;
                Ok(RespCacheable::Cacheable(CacheMeta::new(
                    fresh_until,
                    now,
                    0,
                    0,
                    transient,
                )))
            }
            Shareability::NotShareable(reason) => {
                metrics::BYPASS_REASON
                    .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), reason.as_str()])
                    .inc();
                Ok(RespCacheable::Uncacheable(NoCacheReason::OriginNotCache))
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let backend = self
            .upstreams
            .select(path)
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "no upstream configured"))?;
        // Which of the instances behind this upstream's name. Under
        // `hash_by_path` a path keeps reaching the same instance, whose own
        // render cache then stays warm; otherwise instances take turns.
        let socket = backend
            .socket_for((self.upstreams.strategy() == LoadBalancing::HashByPath).then_some(path));
        // Called again for every retry, which is the point: a retried request
        // goes back through selection and so lands wherever the breakers and
        // the load signal now say it should, rather than back on the backend
        // that just failed it.
        ctx.attempts = ctx.attempts.saturating_add(1);
        ctx.outcome_recorded = false;
        self.retry.record_attempt(self.upstreams.now_ms());
        // Assigning replaces any slot held from a previous attempt, and
        // dropping the old guard hands that backend's slot back.
        self.release_origin_slot(ctx);
        ctx.origin_slot = Some(self.upstreams.enter_selected(backend));
        ctx.upstream = Some(backend.address.clone());
        self.publish_backend_state(backend.id, &backend.address);
        ctx.origin_started = Some(Instant::now());
        ctx.origin_wall_started = Some(std::time::SystemTime::now());
        // The origin fetch gets its own span id, minted here so that the
        // `traceparent` sent upstream names *it* as the parent. Without this
        // the origin's spans would sit beside Tanod's rather than under the
        // fetch, and the origin latency Tanod measures would have nothing
        // to hang off.
        ctx.trace.origin_span_id = Some(SpanId::random());
        metrics::ORIGIN_REQUESTS
            .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), &backend.address])
            .inc();
        let origin = &ctx.policy.config.origin;
        let mut peer = match &origin.tls {
            Some(tls) => {
                let mut peer = HttpPeer::new(socket, true, tls.sni.clone());
                peer.options.verify_cert = tls.verify_cert;
                peer.options.verify_hostname = tls.verify_hostname;
                peer
            }
            None => HttpPeer::new(socket, false, String::new()),
        };
        peer.options.alpn = match origin.http_version {
            OriginHttpVersion::Http1 => pingora_core::protocols::ALPN::H1,
            // Over cleartext this is prior-knowledge h2c: there is no ALPN on
            // the wire, and Pingora reads the minimum version as the caller
            // asserting the origin speaks it.
            OriginHttpVersion::Http2 => pingora_core::protocols::ALPN::H2,
            OriginHttpVersion::Auto => pingora_core::protocols::ALPN::H2H1,
        };
        configure_peer_timeouts(&mut peer, &ctx.policy.config.timeouts);
        Ok(Box::new(peer))
    }

    /// A connect failure: nothing was written upstream, nothing was written
    /// downstream, and the backend is demonstrably not answering.
    ///
    /// This is the one error where Tanod turns a retry *on*. Everywhere
    /// else it can only narrow, because Pingora knows things this layer does
    /// not — chiefly whether a response byte has already been sent.
    fn fail_to_connect(
        &self,
        session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Ctx,
        mut e: Box<Error>,
    ) -> Box<Error> {
        self.record_origin_outcome(ctx, false, "connect");
        e.set_retry(self.decide_retry(session, ctx).allowed());
        e
    }

    /// An error after the connection was established.
    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        ctx: &mut Ctx,
        client_reused: bool,
    ) -> Box<Error> {
        self.record_origin_outcome(ctx, false, "proxy");
        let mut e = e.more_context(format!("Peer: {peer}"));
        // The default implementation's decision, reproduced rather than
        // skipped: only a reused connection may be retried, and only while the
        // retry buffer is intact. It also has to happen before anything reads
        // `retry()`, which panics on an error whose retry is still undecided.
        e.retry
            .decide_reuse(client_reused && !session.as_ref().retry_buffer_truncated());
        // From here Tanod only ever narrows. A `false` is never turned into
        // a `true`.
        if e.retry() && !self.decide_retry(session, ctx).allowed() {
            e.set_retry(false);
        }
        e
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        // `insert_header`, never `append_header`.
        //
        // Appending kept whatever the client sent and added the peer to the
        // end of it, so an origin reading the *first* entry — which is where
        // every framework's `getClientIp` looks — read a value the client
        // chose. Since the origin's own rate limits and audit logs are
        // downstream of this, that is a forged identity with real effects.
        //
        // What Tanod sends is the one address it concluded, and the
        // conclusion already accounts for the hop chain: see
        // [`crate::net::forwarded`].
        match ctx.client.client_ip {
            Some(address) => {
                upstream.insert_header("X-Forwarded-For", address.to_string())?;
            }
            // No address to state. Removing the header is not optional: a
            // stale client-supplied value left in place is exactly the forgery
            // this is here to prevent.
            None => {
                upstream.remove_header("X-Forwarded-For");
            }
        }
        upstream.insert_header("X-Forwarded-Proto", ctx.client.scheme)?;

        // Propagate the context Tanod concluded, never the one that
        // arrived. `insert_header` for the same reason as `X-Forwarded-For`:
        // an appended second `traceparent` is ambiguous, and every runtime
        // resolves the ambiguity differently.
        //
        // The value names the *origin fetch* span, so whatever the origin
        // records nests under the fetch rather than beside it.
        upstream.insert_header(TRACEPARENT, ctx.trace.outbound_traceparent())?;
        match &ctx.trace.tracestate {
            Some(state) => upstream.insert_header(TRACESTATE, state.as_str())?,
            // Nothing believed, so nothing forwarded — and an inbound value
            // that was ignored must not survive into the origin's view.
            None => {
                upstream.remove_header(TRACESTATE);
            }
        }
        // Same reasoning. Tanod does not emit RFC 7239 `Forwarded` itself,
        // so anything arriving under that name is a claim nobody vouched for.
        upstream.remove_header("Forwarded");
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        // Time to first byte, not total response time: it is what reflects the
        // origin's own queueing, and it does not make a backend that served a
        // large body look slow.
        if let (Some(slot), Some(started)) = (&ctx.origin_slot, ctx.origin_started) {
            let id = slot.backend_id();
            self.upstreams.observe_latency(
                id,
                u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            );
            if let Some(address) = ctx.upstream.as_deref() {
                self.publish_backend_state(id, address);
            }
        }
        // Any 5xx is the origin saying it could not do its job, which is the
        // signal a health probe on a static path cannot see. A 4xx is not: the
        // origin answered correctly about a request it was right to refuse.
        self.record_origin_outcome(ctx, !upstream.status.is_server_error(), "status");
        check_origin_deadline(ctx)
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<Option<std::time::Duration>> {
        check_origin_deadline(ctx)?;
        if end_of_stream {
            ctx.origin_finished_ms = ctx
                .origin_started
                .map(|started| started.elapsed().as_millis());
            if ctx.permit.is_some() {
                // Two different claims, and the log has to distinguish them.
                //
                // `origin_end`: this request was spooled and the spool was
                // still absorbing, so no downstream write ever applied
                // backpressure to the origin. End of stream is therefore the
                // moment the origin finished, and the permit is released at
                // the moment it stopped representing work.
                //
                // `body_end`: nothing was spooled, so Pingora paced upstream
                // reads against downstream writes and a slow reader delayed
                // this observation. The permit was still held for real, just
                // for longer than the render took — bounded by
                // `timeouts.downstream_write`.
                //
                // Releasing early on the strength of a `Content-Length` was
                // tried and reverted: a length describes the body's size,
                // never that the origin has finished producing it. See
                // `bench/slowclient.sh`.
                ctx.permit_released_at = Some(match &ctx.spool {
                    Some(spool) if spool.is_active() => "origin_end",
                    _ => "body_end",
                });
            }
            ctx.permit = None;
            // The backend's slot goes back on exactly the same argument, and
            // it has to: least-loaded selection reads this count, and holding
            // it until the client finished reading would make a backend
            // serving slow readers look permanently busy.
            self.release_origin_slot(ctx);
        }
        Ok(None)
    }

    /// The downstream side of the body path, and where the spool lives.
    ///
    /// This runs after Pingora has written the chunk to the cache and
    /// immediately before the write that a slow client would block. Taking the
    /// bytes here rather than in `upstream_response_body_filter` is the whole
    /// trick: the earlier hook runs *before* the cache write, so withholding
    /// there would store an empty entry.
    fn response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<Option<std::time::Duration>> {
        let Some(spool) = ctx.spool.as_mut() else {
            return Ok(None);
        };
        // An upgraded connection is a tunnel in both directions. Buffering it
        // would hold one side's bytes until the other side said something,
        // which for an interactive protocol is a deadlock rather than a delay.
        if session.was_upgraded() {
            return Ok(None);
        }
        let was_active = spool.is_active();
        *body = spool.offer(body.take(), end_of_stream);

        if was_active && !spool.is_active() {
            let outcome = spool.outcome().unwrap_or(SpoolOutcome::Complete);
            ctx.spool_outcome = Some(outcome);
            metrics::SPOOL
                .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), outcome.as_str()])
                .inc();
        }
        metrics::SPOOL_BYTES.set(self.spool_budget.used() as i64);
        Ok(None)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        resp: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        resp.remove_header(crate::cache::TRANSIENT_HEADER);
        // Invalidation tags describe an origin's internal content model —
        // which products, which collections, which revisions. That is
        // reconnaissance, and it is nobody downstream's business, so the
        // header is stripped whether the response came from the origin or
        // from the cache.
        resp.remove_header(self.store.tag_header());
        if ctx.policy.config.debug_headers {
            resp.insert_header("X-Tanod", cache_status(session, ctx).to_ascii_uppercase())?;
        }
        Ok(())
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Ctx) {
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        let route = ctx.route_id.as_deref().unwrap_or("-");
        let cache = cache_status(session, ctx);
        metrics::CACHE.with_label_values(&[route, cache]).inc();

        let origin_ms = match (ctx.origin_finished_ms, ctx.origin_started) {
            (Some(elapsed_ms), _) => {
                metrics::ORIGIN_LATENCY
                    .with_label_values(&[route])
                    .observe(elapsed_ms as f64 / 1000.0);
                elapsed_ms
            }
            (None, Some(started)) => {
                let elapsed = started.elapsed();
                metrics::ORIGIN_LATENCY
                    .with_label_values(&[route])
                    .observe(elapsed.as_secs_f64());
                elapsed.as_millis()
            }
            (None, None) => 0,
        };

        let client = ctx
            .client
            .client_ip
            .map(|address| address.to_string())
            .unwrap_or_else(|| "-".to_string());
        let trace_id = ctx.trace.trace_id.to_hex();
        let span_id = ctx.trace.span_id.to_hex();
        let access = AccessLog {
            method: session.req_header().method.as_str(),
            // Path only — the query string routinely carries tokens.
            path: session.req_header().uri.path(),
            route,
            class: ctx.class.as_str(),
            cache,
            upstream: ctx.upstream.as_deref(),
            client: &client,
            scheme: ctx.client.scheme,
            status,
            shed: ctx.shed,
            origin_ms,
            total_ms: ctx.started.elapsed().as_millis(),
            permit_released_at: ctx.permit_released_at.unwrap_or("-"),
            spool: ctx.spool_outcome.map(SpoolOutcome::as_str).unwrap_or("-"),
            trace_id: &trace_id,
            span_id: &span_id,
            trace_continued: ctx.trace.continued,
            generation: ctx.policy.generation,
        };
        let line = match ctx.policy.config.telemetry.logging.format {
            LogFormat::Json => access.to_json(),
            LogFormat::Text => access.to_text(),
        };
        log::info!("{line}");

        // Spans are built only when someone is listening and the trace was
        // sampled. Everything above happens on every request; this is the one
        // part that is allowed to be conditional, because it is the one part
        // that costs allocation.
        if let Some(sink) = self.spans.as_ref().filter(|_| ctx.trace.sampled) {
            self.record_spans(sink, session, ctx, &access, status);
        }

        // Normally already released at upstream end-of-stream; this covers
        // the paths that never got there (shed, cache hit, upstream error,
        // and a client that disconnected mid-body). Then republish the gauges:
        // updating them only on the way in leaves `in_flight` reading stale
        // while the proxy is idle, which is the moment an operator is most
        // likely to be looking at it.
        ctx.permit = None;
        // Dropping the spool returns its share of the global byte budget. On
        // the disconnect path this is the only thing that does — end of stream
        // never arrives — so it happens here rather than nowhere.
        ctx.spool = None;
        metrics::SPOOL_BYTES.set(self.spool_budget.used() as i64);
        // Published on the way out rather than from a timer, for the same
        // reason as the limiter gauges: a sampled memory number misses the
        // spike that caused the incident someone is reading it during.
        metrics::CACHE_BYTES.set(self.store.bytes_used() as i64);
        metrics::CACHE_ENTRIES.set(self.store.entries() as i64);
        metrics::CACHE_TAGS.set(self.store.tags() as i64);
        if ctx.upgrade_permit.take().is_some() {
            metrics::UPGRADES_ACTIVE.set(
                (self
                    .upgrades
                    .limit()
                    .saturating_sub(self.upgrades.available())) as i64,
            );
        }
        let global = self.admission.global();
        metrics::IN_FLIGHT
            .with_label_values(&["global"])
            .set((global.limit().saturating_sub(global.available())) as i64);
        metrics::QUEUE_DEPTH
            .with_label_values(&["global"])
            .set(global.queue_depth() as i64);
        if let Some(l) = ctx
            .route_id
            .as_deref()
            .and_then(|id| self.limiter_for(&ctx.policy, id))
        {
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
            metrics::QUEUE_DEPTH
                .with_label_values(&[l.name()])
                .set(l.queue_depth() as i64);
        }
        self.publish_tier_state(ctx.priority);
        // Republish what least-loaded selection reads, so the routing decision
        // is auditable rather than a black box. `release_origin_slot` drops the
        // guard before publishing, so the gauge is the post-release value.
        self.release_origin_slot(ctx);
    }
}

/// A header value as text, or `None`.
///
/// `HeaderValue::to_str` refuses obs-text (`0x80..=0xff`) that `HeaderValue`
/// itself accepts, and elsewhere in this crate dropping the value on that
/// failure was a real bug — a single non-ASCII byte hid an entire `Cookie`
/// header. It is the correct behaviour *here* and nowhere else: the only
/// callers are `traceparent` and `tracestate`, both of which are specified as
/// printable ASCII, so a value that is not readable as UTF-8 is a value that
/// could never have parsed. Treating it as absent starts a fresh trace, which
/// is the same thing a malformed value does.
/// The request path, if it is short enough to be worth remembering.
///
/// Every stored path costs memory inside the cache's byte budget, and a path
/// is client-controlled, so this is bounded like everything else that is. A
/// path past the bound is not purgeable *by path* — it is still purgeable by
/// tag and by `all`. Truncating instead would be worse than dropping: a
/// truncated path can collide with a different one, and a purge that removes
/// the wrong entry is a correctness bug rather than a missing feature.
fn purgeable_path(path: &str) -> &str {
    if path.len() <= crate::cache::MAX_PURGEABLE_PATH {
        path
    } else {
        ""
    }
}

fn header_str<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The authority this request names, over either protocol version.
///
/// HTTP/1.1 puts it in `Host`. HTTP/2 abolished that header and replaced it
/// with the `:authority` pseudo-header, which Pingora surfaces on the URI
/// rather than in the header map — so reading `Host` alone answers "" for
/// every h2 request, and an empty host in the cache key merges every virtual
/// host on the listener into one entry. That is a cross-tenant response leak
/// that appears the moment `server.h2c` or `server.tls` is switched on and
/// affects nothing before then, which is exactly the kind of change that
/// reaches production unnoticed.
///
/// Rendered, never dropped: a `Host` whose bytes `to_str` refuses used to
/// collapse to the empty string, with the same consequence.
fn request_host(req: &RequestHeader) -> String {
    if let Some(host) = req.headers.get(http::header::HOST) {
        return crate::classifier::header_text(host);
    }
    req.uri
        .authority()
        .map(|authority| authority.as_str().to_string())
        .unwrap_or_default()
}

/// Which scheme is this connection actually speaking?
///
/// Read from the connection's own TLS digest rather than from configuration,
/// because a process can serve both a cleartext and a TLS listener at once and
/// the answer differs per connection. This is the unforgeable half of the
/// scheme decision; the forgeable half is the forwarded header, and
/// [`TrustPolicy`] decides whether to believe it.
fn listener_scheme(session: &Session) -> ListenerScheme {
    let is_tls = session
        .digest()
        .is_some_and(|digest| digest.ssl_digest.is_some());
    if is_tls {
        ListenerScheme::Https
    } else {
        ListenerScheme::Http
    }
}

fn nonzero(duration: std::time::Duration) -> Option<std::time::Duration> {
    (!duration.is_zero()).then_some(duration)
}

fn configure_peer_timeouts(peer: &mut HttpPeer, timeouts: &Timeouts) {
    peer.options.connection_timeout = nonzero(timeouts.connect.as_duration());
    peer.options.total_connection_timeout = nonzero(timeouts.connect.as_duration());
    peer.options.idle_timeout = nonzero(timeouts.idle.as_duration());
    peer.options.read_timeout = [
        timeouts.first_byte.as_duration(),
        timeouts.idle.as_duration(),
        timeouts.origin.as_duration(),
    ]
    .into_iter()
    .filter(|duration| !duration.is_zero())
    .min();
}

fn resolved_key_headers(framework: &[&str], route: Option<&RouteCache>) -> Vec<String> {
    let mut headers = framework
        .iter()
        .map(|header| (*header).to_string())
        .collect::<Vec<_>>();
    if let Some(vary) = route.and_then(|cache| cache.vary.as_ref()) {
        headers.extend(vary.headers.iter().cloned());
    }
    for header in &mut headers {
        header.make_ascii_lowercase();
    }
    headers.sort_unstable();
    headers.dedup();
    headers
}

/// Resolve the request class without allowing route policy to erase protocol
/// facts. A route declaration can improve framework-specific knowledge for an
/// ordinary HTTP request, but an Upgrade header means the request is asking to
/// become a tunnel regardless of which path matched.
fn resolved_request_class(
    req: &RequestMetadata<'_>,
    declared: Option<RequestClass>,
    inferred: Option<RequestClass>,
) -> RequestClass {
    if req.is_upgrade() {
        RequestClass::Upgrade
    } else {
        declared.or(inferred).unwrap_or(RequestClass::Unknown)
    }
}

fn check_origin_deadline(ctx: &Ctx) -> Result<()> {
    let timeout = ctx.policy.config.timeouts.origin.as_duration();
    if !timeout.is_zero()
        && ctx
            .origin_started
            .is_some_and(|started| started.elapsed() >= timeout)
    {
        return Err(Error::create(
            ErrorType::ReadTimedout,
            pingora_core::ErrorSource::Upstream,
            None,
            Some("origin request exceeded timeouts.origin".into()),
        ));
    }
    Ok(())
}

/// One vocabulary for the cache outcome, shared by the debug header, the
/// access log and the metrics, so the three can never disagree.
fn cache_status(session: &Session, ctx: &Ctx) -> &'static str {
    match session.cache.phase() {
        pingora_cache::CachePhase::Hit => "hit",
        pingora_cache::CachePhase::Stale | pingora_cache::CachePhase::StaleUpdating => "stale",
        pingora_cache::CachePhase::Miss | pingora_cache::CachePhase::Expired => "miss",
        pingora_cache::CachePhase::Bypass => "bypass",
        _ if ctx.shed => "shed",
        _ if ctx.cache_active => "miss",
        _ => "disabled",
    }
}

fn joined_header_values(
    headers: &http::HeaderMap,
    name: http::header::HeaderName,
) -> Option<String> {
    let values = headers
        .get_all(name)
        .iter()
        .map(crate::classifier::header_text)
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::VaryPolicy;

    /// A request shaped the way Pingora hands one over from an HTTP/2
    /// session: `:authority` on the URI, no `Host` header anywhere.
    fn h2_request(authority: &str, path: &str) -> RequestHeader {
        let mut req = RequestHeader::build("GET", path.as_bytes(), None).unwrap();
        req.set_uri(
            http::Uri::builder()
                .scheme("https")
                .authority(authority)
                .path_and_query(path)
                .build()
                .unwrap(),
        );
        req
    }

    #[test]
    fn the_authority_is_found_over_both_protocol_versions() {
        // HTTP/1.1: the `Host` header.
        let mut h1 = RequestHeader::build("GET", b"/products/x", None).unwrap();
        h1.insert_header("host", "shop.example.com").unwrap();
        assert_eq!(request_host(&h1), "shop.example.com");

        // HTTP/2: no `Host` at all — the authority lives on the URI, exactly
        // as Pingora surfaces `:authority`. Reading only the header answers
        // "", and an empty host in the cache key puts every virtual host on
        // the listener into one entry.
        let h2 = h2_request("shop.example.com", "/products/x");
        assert!(
            h2.headers.get(http::header::HOST).is_none(),
            "this test is meaningless if the builder synthesised a Host header"
        );
        assert_eq!(request_host(&h2), "shop.example.com");

        // Neither: still not a panic, and still not a shared key by accident —
        // an empty host is at least honest about being empty.
        let bare = RequestHeader::build("GET", b"/x", None).unwrap();
        assert_eq!(request_host(&bare), "");
    }

    #[test]
    fn two_authorities_never_produce_one_key_over_http2() {
        let first = h2_request("a.example.com", "/p");
        let second = h2_request("b.example.com", "/p");
        assert_ne!(request_host(&first), request_host(&second));
    }

    #[test]
    fn route_vary_headers_are_merged_with_framework_variants() {
        let route = RouteCache {
            vary: Some(VaryPolicy {
                headers: vec!["Accept-Language".into(), "RSC".into()],
            }),
            ..Default::default()
        };
        assert_eq!(
            resolved_key_headers(&["rsc", "next-url"], Some(&route)),
            ["accept-language", "next-url", "rsc"]
        );
    }

    #[test]
    fn repeated_cache_policy_headers_are_combined() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::CACHE_CONTROL,
            "public, max-age=60".parse().unwrap(),
        );
        headers.append(http::header::CACHE_CONTROL, "private".parse().unwrap());
        assert_eq!(
            joined_header_values(&headers, http::header::CACHE_CONTROL).as_deref(),
            Some("public, max-age=60,private")
        );
    }

    #[test]
    fn a_route_class_cannot_erase_an_upgrade() {
        let method = http::Method::GET;
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        let req = RequestMetadata {
            method: &method,
            host: "shop.example.com",
            path: "/socket",
            query: None,
            headers: &headers,
        };

        for declared in [
            RequestClass::Static,
            RequestClass::PublicDocument,
            RequestClass::PublicDynamic,
            RequestClass::PrivateDynamic,
            RequestClass::Streaming,
        ] {
            assert_eq!(
                resolved_request_class(&req, Some(declared), Some(RequestClass::Upgrade)),
                RequestClass::Upgrade,
                "{declared:?} route bypassed the upgrade policy"
            );
        }
    }

    #[test]
    fn every_origin_timeout_is_applied_to_the_peer() {
        let timeouts = Timeouts::default();
        let mut peer = HttpPeer::new("127.0.0.1:3000", false, String::new());
        configure_peer_timeouts(&mut peer, &timeouts);
        assert_eq!(
            peer.options.connection_timeout,
            Some(timeouts.connect.as_duration())
        );
        assert_eq!(
            peer.options.total_connection_timeout,
            Some(timeouts.connect.as_duration())
        );
        assert_eq!(peer.options.idle_timeout, Some(timeouts.idle.as_duration()));
        assert_eq!(
            peer.options.read_timeout,
            Some(timeouts.first_byte.as_duration())
        );
    }
}
