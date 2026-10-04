//! The operator surface: liveness, readiness and status.
//!
//! # Why this is a separate listener
//!
//! Not a path on the traffic listener, for two reasons. `/status` is a
//! perfectly ordinary application route, so a path would collide with the
//! origin's own URL space and the collision would be silent. And it would
//! publish backend health, cache occupancy and the configuration generation to
//! anyone who can reach the site — a reconnaissance gift, and on a governor
//! whose whole job is to be the thing that stays up during an incident.
//!
//! Bind it to loopback or a private address. Tanod refuses to start if it is
//! bound to the same address as the traffic listener; it cannot tell whether
//! `0.0.0.0` is safe on your network, and says so in `tanod check` instead.
//!
//! # No client-controlled cardinality
//!
//! Nothing here is parameterised. There is no path segment, query key, header
//! or body that changes what is computed or how much of it there is. Every
//! response is built from a fixed set of fields whose sizes are bounded by the
//! configuration file: route ids, upstream addresses, limiter names. This is
//! the same rule the Prometheus labels follow, for the same reason — an
//! operator surface must never be a way to make the process do unbounded work,
//! least of all during the incident it exists to explain.
//!
//! # Readiness is a claim about *this instance*
//!
//! `/health/ready` answers 503 while draining, so a load balancer stops
//! sending new work before the process starts shutting down. That gap is the
//! whole reason zero-downtime restarts work, and it is why draining is a state
//! Tanod enters some seconds *before* it begins to exit.

pub mod drain;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use http::{Response, StatusCode};
use percent_encoding::percent_decode_str;
use pingora_core::apps::http_app::ServeHttp;
use pingora_core::protocols::http::ServerSession;

use crate::admission::AdmissionController;
use crate::admission::limiter::Limiter;
use crate::cache::BoundedStore;
use crate::policy::PolicySnapshot;
use crate::proxy::spool::SpoolBudget;
use crate::telemetry::json::{field_str, quoted};
use crate::upstream::UpstreamPool;
use crate::upstream::breaker::BreakerState;
use crate::upstream::retry::RetryBudget;
use drain::DrainState;

/// Everything the admin endpoints read. Every field is a handle to live state,
/// never a copy — a status document assembled from a snapshot taken at startup
/// is worse than none at all.
pub struct Admin {
    pub started: Instant,
    pub config_path: String,
    pub policy: Arc<ArcSwap<PolicySnapshot>>,
    pub admission: Arc<AdmissionController>,
    pub upstreams: Arc<UpstreamPool>,
    pub store: &'static BoundedStore,
    pub spool: Arc<SpoolBudget>,
    pub upgrades: Arc<Limiter>,
    pub retry: Arc<RetryBudget>,
    /// Shared secret for `POST /purge`. `None` disables the endpoint outright.
    pub purge_token: Option<String>,
    pub drain: Arc<DrainState>,
    /// Report not-ready when no upstream is passing its health check. Off by
    /// default; see [`crate::config::schema::Admin`].
    pub require_healthy_upstream: bool,
}

/// Why readiness said no. A closed set, so it is safe to put in a response and
/// safe to alert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    Draining,
    NoHealthyUpstream,
}

impl NotReady {
    pub fn as_str(self) -> &'static str {
        match self {
            NotReady::Draining => "draining",
            NotReady::NoHealthyUpstream => "no_healthy_upstream",
        }
    }
}

impl Admin {
    /// The readiness decision, separated from its HTTP rendering so it can be
    /// tested without a socket.
    pub fn readiness(&self) -> Result<(), NotReady> {
        if self.drain.is_draining() {
            return Err(NotReady::Draining);
        }
        if self.require_healthy_upstream && self.upstreams.healthy_count() == 0 {
            return Err(NotReady::NoHealthyUpstream);
        }
        Ok(())
    }

    fn liveness_body(&self) -> String {
        let mut s = String::from("{");
        field_str(&mut s, "status", "live");
        field_str(&mut s, "version", env!("CARGO_PKG_VERSION"));
        let _ = write!(
            s,
            "\"uptime_seconds\":{}}}",
            self.started.elapsed().as_secs()
        );
        s
    }

    fn readiness_body(&self) -> (StatusCode, String) {
        let verdict = self.readiness();
        let mut s = String::from("{");
        let _ = write!(s, "\"ready\":{},", verdict.is_ok());
        field_str(
            &mut s,
            "reason",
            verdict.err().map_or("ok", NotReady::as_str),
        );
        let _ = write!(
            s,
            "\"draining\":{},\"healthy_upstreams\":{},\"upstreams\":{}}}",
            self.drain.is_draining(),
            self.upstreams.healthy_count(),
            self.upstreams.len()
        );
        let status = if verdict.is_ok() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        (status, s)
    }

    /// Authorise and perform a purge, returning the JSON body on success.
    ///
    /// Authorisation happens before parsing, so an unauthenticated caller
    /// cannot use error messages to learn which parameters this build accepts.
    fn purge(
        &self,
        authorization: Option<&[u8]>,
        query: Option<&str>,
    ) -> Result<String, PurgeRefusal> {
        let Some(token) = self.purge_token.as_deref() else {
            return Err(PurgeRefusal::Disabled);
        };
        let presented = authorization
            .and_then(|value| value.strip_prefix(b"Bearer "))
            .ok_or(PurgeRefusal::Unauthorized)?;
        if !secret_eq(presented, token.as_bytes()) {
            return Err(PurgeRefusal::Unauthorized);
        }

        let request = parse_purge_query(query)?;
        let (purged, scope, tags, paths) = match &request {
            PurgeRequest::All => (self.store.purge_all(), "all", 0, 0),
            PurgeRequest::Selective { tags, paths } => {
                // Two passes over one request, and the counts are summed
                // rather than the sets unioned first: an entry carrying a
                // purged tag *and* answering a purged path is removed once,
                // because the second pass no longer finds it.
                let mut purged = self.store.purge_tags(tags.iter().map(String::as_str));
                let by_path = self.store.purge_paths(paths.iter().map(String::as_str));
                purged.entries += by_path.entries;
                purged.bytes += by_path.bytes;
                (purged, "selective", tags.len(), paths.len())
            }
        };
        crate::telemetry::metrics::CACHE_PURGED
            .with_label_values(&[scope])
            .inc_by(purged.entries as u64);
        crate::telemetry::metrics::CACHE_BYTES.set(self.store.bytes_used() as i64);
        crate::telemetry::metrics::CACHE_ENTRIES.set(self.store.entries() as i64);
        crate::telemetry::metrics::CACHE_TAGS.set(self.store.tags() as i64);
        log::info!(
            "purge scope={scope} tags={tags} paths={paths} entries={} bytes={}",
            purged.entries,
            purged.bytes
        );

        let mut body = String::from("{\"purged\":true,");
        field_str(&mut body, "scope", scope);
        let _ = write!(
            body,
            "\"tags\":{tags},\"paths\":{paths},\"entries\":{},\"bytes\":{},\
             \"remaining_entries\":{}}}",
            purged.entries,
            purged.bytes,
            self.store.entries()
        );
        Ok(body)
    }

    /// The full status document.
    ///
    /// Everything in it is either a constant, a number, or a string that came
    /// from the configuration file. Nothing a client sent reaches it.
    pub fn status_body(&self) -> String {
        let policy = self.policy.load();
        let cfg = &policy.config;
        let mut s = String::with_capacity(1024);
        s.push('{');
        field_str(&mut s, "version", env!("CARGO_PKG_VERSION"));
        let _ = write!(
            s,
            "\"uptime_seconds\":{},",
            self.started.elapsed().as_secs()
        );

        // ---- configuration
        s.push_str("\"config\":{");
        field_str(&mut s, "path", &self.config_path);
        field_str(
            &mut s,
            "mode",
            match cfg.mode {
                crate::config::schema::Mode::Observe => "observe",
                crate::config::schema::Mode::Protect => "protect",
            },
        );
        let _ = write!(
            s,
            "\"schema_version\":{},\"generation\":{},\"fingerprint\":{},\"routes\":{},\"capacity\":",
            crate::config::SCHEMA_VERSION,
            policy.generation,
            policy.fingerprint,
            cfg.routes.len()
        );
        match &cfg.capacity {
            Some(group) => {
                let allocated = cfg.origin.concurrency.max * group.replicas;
                let _ = write!(
                    s,
                    "{{\"global_max\":{},\"replicas\":{},\"per_replica\":{},\"allocated\":{allocated}}}",
                    group.global_max, group.replicas, cfg.origin.concurrency.max
                );
            }
            None => s.push_str("null"),
        }
        s.push_str(",\"features\":[");
        let mut first = true;
        for feature in compiled_features() {
            if !first {
                s.push(',');
            }
            quoted(&mut s, feature);
            first = false;
        }
        s.push_str("]},");

        // ---- drain
        s.push_str("\"drain\":{");
        let _ = write!(s, "\"draining\":{},", self.drain.is_draining());
        match self.drain.draining_for() {
            Some(d) => {
                let _ = write!(s, "\"draining_for_seconds\":{},", d.as_secs());
            }
            None => s.push_str("\"draining_for_seconds\":null,"),
        }
        field_str(&mut s, "reason", self.drain.reason());
        s.truncate(s.trim_end_matches(',').len());
        s.push_str("},");

        // ---- admission
        s.push_str("\"admission\":{\"global\":");
        limiter_json(&mut s, self.admission.global());
        s.push_str(",\"tiers\":[");
        for (i, limiter) in self.admission.tier_limiters().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            limiter_json(&mut s, limiter);
        }
        s.push_str("],\"routes\":[");
        for (i, limiter) in self.admission.route_limiters().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            limiter_json(&mut s, limiter);
        }
        s.push_str("]},");

        // ---- upstreams
        //
        // `healthy` and `ejected` are separate answers to separate questions,
        // and a backend can be both healthy and ejected at once: that is what
        // an origin answering its probe while failing its renders looks like,
        // and collapsing the two into one field would hide the distinction
        // that makes passive observation worth having.
        s.push_str("\"upstreams\":[");
        for (i, backend) in self.upstreams.backends().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let (ok, failed, trips) = self.upstreams.breaker_counts(backend.id);
            s.push('{');
            field_str(&mut s, "address", &backend.address);
            let _ = write!(
                s,
                "\"healthy\":{},\"ejected\":{},\"in_flight\":{},\"latency_ewma_us\":{},\
                 \"window\":{{\"ok\":{ok},\"failed\":{failed}}},\"trips\":{trips}}}",
                self.upstreams.is_healthy(backend.id),
                self.upstreams.breaker_state(backend.id) == BreakerState::Open,
                self.upstreams.in_flight(backend.id),
                self.upstreams.ewma_micros(backend.id),
            );
        }
        let _ = write!(
            s,
            "],\"cache\":{{\"enabled\":{},\"bytes_used\":{},\"max_bytes\":{},\"entries\":{},",
            cfg.cache.enabled,
            self.store.bytes_used(),
            self.store.limit(),
            self.store.entries()
        );
        field_str(
            &mut s,
            "eviction",
            match self.store.eviction() {
                crate::config::schema::Eviction::Clock => "clock",
                crate::config::schema::Eviction::Fifo => "fifo",
            },
        );
        field_str(&mut s, "tag_header", self.store.tag_header());
        let _ = write!(
            s,
            "\"tags\":{},\"evicted\":{},\"purged\":{},\"purge_enabled\":{}}},",
            self.store.tags(),
            self.store.evicted(),
            self.store.purged(),
            self.purge_token.is_some()
        );

        let _ = write!(
            s,
            "\"spool\":{{\"bytes_used\":{},\"max_bytes\":{}}},",
            self.spool.used(),
            self.spool.limit()
        );
        {
            let now = self.upstreams.now_ms();
            let (attempts, retries) = self.retry.counts(now);
            let _ = write!(
                s,
                "\"retry\":{{\"enabled\":{},\"max_attempts\":{},\"window_attempts\":{attempts},\
                 \"window_retries\":{retries},\"budget\":{}}},",
                self.retry.enabled(),
                self.retry.max_attempts(),
                self.retry.allowance(now),
            );
        }
        let _ = write!(
            s,
            "\"upgrades\":{{\"enabled\":{},\"limit\":{},\"in_flight\":{}}}}}",
            cfg.upgrade.enabled,
            self.upgrades.limit(),
            self.upgrades
                .limit()
                .saturating_sub(self.upgrades.available())
        );
        s
    }
}

/// Which optional features this binary was compiled with.
///
/// In the status document because "is TLS available in this build" is
/// otherwise only answerable by trying it, and the answer differs between a
/// container image and a local `cargo build`.
fn compiled_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "tls") {
        features.push("tls");
    }
    features
}

fn limiter_json(s: &mut String, limiter: &Limiter) {
    s.push('{');
    field_str(s, "name", limiter.name());
    let limit = limiter.limit();
    let _ = write!(
        s,
        "\"limit\":{},\"in_flight\":{},\"available\":{},\"queue_depth\":{},\
         \"queue_max\":{},\"queue_timeout_ms\":{}}}",
        limit,
        limit.saturating_sub(limiter.available()),
        limiter.available(),
        limiter.queue_depth(),
        limiter.queue_max(),
        limiter.queue_timeout().as_millis()
    );
}

const INDEX: &str = concat!(
    "tanod ",
    env!("CARGO_PKG_VERSION"),
    " admin\n\n",
    "GET  /health/live   always 200 while the process is running\n",
    "GET  /health/ready  503 while draining or, if configured, while no upstream is healthy\n",
    "GET  /status        configuration generation, backend state, cache and spool usage\n",
    "POST /purge         invalidate cache entries by tag or path, or all of them\n",
    "                    ?tag=<name> and/or ?path=</url> (repeatable) | ?all=1\n",
    "                    requires: Authorization: Bearer <cache.purge.token>\n",
);

/// The most `?tag=` and `?path=` parameters one purge may name, each.
///
/// The request line is already bounded by Pingora's header limit, so this is
/// not the only bound — it is the one that says what the limit is on purpose
/// rather than leaving it to be discovered.
const MAX_PURGE_TAGS: usize = 128;

/// What a purge request asked for.
#[derive(Debug, PartialEq, Eq)]
enum PurgeRequest {
    /// Tags and paths may be combined: a deploy hook that calls both
    /// `revalidateTag()` and `revalidatePath()` should be one request, not two
    /// round trips and two chances to half-apply.
    Selective {
        tags: Vec<String>,
        paths: Vec<String>,
    },
    All,
}

/// Why a purge request was refused. Every variant is a response body, so none
/// of them may echo anything the caller sent.
#[derive(Debug, PartialEq, Eq)]
enum PurgeRefusal {
    /// No token configured, so the endpoint does not exist.
    Disabled,
    /// Missing or wrong credentials.
    Unauthorized,
    /// Nothing to do, or contradictory instructions.
    Malformed(&'static str),
}

/// Compare two secrets without leaking their contents through timing.
///
/// `==` on a `str` short-circuits at the first differing byte, which over
/// enough requests tells an attacker how much of a token prefix they have
/// right. The lengths are compared first and unequal lengths are refused
/// immediately: the length of a configured token is not the secret, and
/// padding to hide it would be theatre.
fn secret_eq(provided: &[u8], expected: &[u8]) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in provided.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Parse `?tag=…&tag=…` / `?all=1` out of a query string.
///
/// Values are percent-decoded exactly once. A cache tag is opaque, so a literal
/// `%` is represented as `%25`; decoding is what also lets reserved bytes such
/// as `&` and `#` be named without turning them into query syntax.
fn parse_purge_query(query: Option<&str>) -> Result<PurgeRequest, PurgeRefusal> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Err(PurgeRefusal::Malformed(
            "say what to purge: ?tag=<name>, ?path=</url>, or ?all=1",
        ));
    };
    let mut tags: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let mut all = false;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| PurgeRefusal::Malformed("parameter values must be valid UTF-8"))?;
        let value = value.as_ref();
        match name {
            "tag" if !value.is_empty() => {
                if tags.len() >= MAX_PURGE_TAGS {
                    return Err(PurgeRefusal::Malformed("too many tags in one request"));
                }
                if !tags.iter().any(|existing| existing == value) {
                    tags.push(value.to_string());
                }
            }
            // Absolute paths only. A relative one could never match a stored
            // entry — the stored value is the request path, which always
            // begins with `/` — so accepting it would be a purge that silently
            // removed nothing.
            "path" if value.starts_with('/') => {
                if paths.len() >= MAX_PURGE_TAGS {
                    return Err(PurgeRefusal::Malformed("too many paths in one request"));
                }
                if !paths.iter().any(|existing| existing == value) {
                    paths.push(value.to_string());
                }
            }
            "path" => {
                return Err(PurgeRefusal::Malformed(
                    "path must be absolute and begin with /",
                ));
            }
            "all" if value == "1" || value == "true" => all = true,
            // An unknown parameter is refused rather than ignored, on the same
            // reasoning as `deny_unknown_fields` in the config: a typo'd
            // `?tags=` that quietly purged nothing would look like a working
            // invalidation for as long as nobody checked.
            _ => {
                return Err(PurgeRefusal::Malformed(
                    "unrecognised parameter; expected tag or all",
                ));
            }
        }
    }
    let selective = !tags.is_empty() || !paths.is_empty();
    match (all, selective) {
        (true, false) => Ok(PurgeRequest::All),
        (false, true) => Ok(PurgeRequest::Selective { tags, paths }),
        (true, true) => Err(PurgeRefusal::Malformed(
            "all=1 cannot be combined with tag= or path=; pick one",
        )),
        (false, false) => Err(PurgeRefusal::Malformed(
            "say what to purge: ?tag=<name>, ?path=</url>, or ?all=1",
        )),
    }
}

#[async_trait]
impl ServeHttp for Admin {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let req = session.req_header();
        let path = req.uri.path();

        // Purge is the one endpoint here that changes anything, so it is the
        // one endpoint that is not a `GET`. That is not decoration: a `GET`
        // that invalidates a cache gets fetched by link prefilers, crawlers,
        // browser history and `curl` in a shell loop, and every one of those
        // becomes an origin stampede.
        if path == "/purge" {
            if req.method != http::Method::POST {
                return text(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "purge is POST only; a GET that invalidates a cache is a stampede \
                     waiting for a crawler\n",
                );
            }
            let authorization = req
                .headers
                .get(http::header::AUTHORIZATION)
                .map(http::HeaderValue::as_bytes);
            return match self.purge(authorization, req.uri.query()) {
                Ok(body) => json(StatusCode::OK, body),
                Err(PurgeRefusal::Disabled) => text(
                    StatusCode::NOT_FOUND,
                    "purge is not configured; set cache.purge.token to enable it\n",
                ),
                Err(PurgeRefusal::Unauthorized) => unauthorized(),
                Err(PurgeRefusal::Malformed(why)) => {
                    text(StatusCode::BAD_REQUEST, &format!("{why}\n"))
                }
            };
        }

        // A `HEAD` is answered like a `GET` minus the body, which Pingora does
        // by way of the response it is handed; anything else is refused
        // outright. Everything below has no state to change.
        if req.method != http::Method::GET && req.method != http::Method::HEAD {
            return text(
                StatusCode::METHOD_NOT_ALLOWED,
                "only GET and HEAD are served here\n",
            );
        }
        // Path only. A query string cannot change anything below, and is
        // ignored rather than rejected so that a `?v=1` cache-buster from a
        // dashboard does not read as an outage.
        match path {
            "/health/live" | "/healthz" => json(StatusCode::OK, self.liveness_body()),
            "/health/ready" | "/readyz" => {
                let (status, body) = self.readiness_body();
                json(status, body)
            }
            "/status" => json(StatusCode::OK, self.status_body()),
            "/" => text(StatusCode::OK, INDEX),
            _ => text(StatusCode::NOT_FOUND, "not found\n"),
        }
    }
}

/// A refusal that says how to authenticate without saying anything about the
/// secret, and that a cache is never allowed to store.
fn unauthorized() -> Response<Vec<u8>> {
    let mut response = text(
        StatusCode::UNAUTHORIZED,
        "purge requires Authorization: Bearer <cache.purge.token>\n",
    );
    response.headers_mut().insert(
        http::header::WWW_AUTHENTICATE,
        http::HeaderValue::from_static("Bearer realm=\"tanod-purge\""),
    );
    response
}

fn json(status: StatusCode, body: String) -> Response<Vec<u8>> {
    build(status, "application/json", body.into_bytes())
}

fn text(status: StatusCode, body: &str) -> Response<Vec<u8>> {
    build(
        status,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
    )
}

/// `no-store` on every answer.
///
/// A cached readiness response is a load balancer routing to an instance that
/// stopped being ready some seconds ago, which is the exact failure this
/// endpoint exists to prevent.
fn build(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::CACHE_CONTROL, "no-store")
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        // The builder fails only on an invalid status or header, all of which
        // are constants here. A 500 with no body is still a truthful answer,
        // and it keeps the admin surface from being the one thing that can
        // panic a proxy.
        .unwrap_or_else(|_| {
            let mut fallback = Response::new(Vec::new());
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::LoadBalancing;
    use std::time::Duration;

    fn admin(require_healthy: bool) -> Admin {
        let yaml = "version: 1\norigin:\n  upstreams: [\"127.0.0.1:3000\", \"127.0.0.2:3000\"]\n\
                    routes:\n  - id: products\n    match: \"/products/**\"\n    concurrency:\n      max: 4\n";
        let cfg: crate::config::Config = serde_saphyr::from_str(yaml).unwrap();
        crate::config::validation::validate(&cfg).unwrap();
        let policy = PolicySnapshot::build(cfg, 7).unwrap();
        let admission = Arc::new(AdmissionController::new(
            10,
            5,
            Duration::from_millis(250),
            &crate::config::schema::Priorities::default(),
        ));
        admission.route_limiter("products", 4, 2, Duration::from_millis(100));
        Admin {
            started: Instant::now(),
            config_path: "/etc/tanod/tanod.yaml".to_string(),
            policy: Arc::new(ArcSwap::from(policy)),
            admission,
            upstreams: Arc::new({
                let pool = UpstreamPool::new(
                    &["127.0.0.1:3000".to_string(), "127.0.0.2:3000".to_string()],
                    LoadBalancing::RoundRobin,
                    &crate::config::schema::Breaker::default(),
                )
                .unwrap();
                pool.assume_healthy();
                pool
            }),
            store: BoundedStore::new(&crate::config::schema::CacheDefaults {
                max_memory: crate::config::units::Bytes(64 * 1024 * 1024),
                ..Default::default()
            }),
            spool: SpoolBudget::new(1024 * 1024),
            upgrades: Limiter::new("upgrade", 100, 0, Duration::ZERO),
            retry: Arc::new(RetryBudget::new(&crate::config::schema::Retry::default())),
            purge_token: None,
            drain: Arc::new(DrainState::new()),
            require_healthy_upstream: require_healthy,
        }
    }

    // ------------------------------------------------------------- purge

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn admin_with_purge() -> Admin {
        let mut a = admin(false);
        a.purge_token = Some(TOKEN.to_string());
        a
    }

    fn bearer(token: &str) -> Vec<u8> {
        format!("Bearer {token}").into_bytes()
    }

    #[test]
    fn purge_does_not_exist_without_a_token() {
        // Not "unauthorized" — absent. An endpoint that advertises itself as
        // present-but-locked is an invitation to guess at the lock.
        let a = admin(false);
        assert_eq!(
            a.purge(Some(&bearer(TOKEN)), Some("all=1")),
            Err(PurgeRefusal::Disabled)
        );
    }

    #[test]
    fn purge_refuses_a_missing_or_wrong_token() {
        let a = admin_with_purge();
        for authorization in [
            None,
            Some(bearer("")),
            Some(bearer("wrong")),
            // Right length, wrong content — the case a length check alone
            // would wave through.
            Some(bearer(&"f".repeat(TOKEN.len()))),
            // Correct token, wrong scheme.
            Some(format!("Basic {TOKEN}").into_bytes()),
            // Correct token as a bare value with no scheme.
            Some(TOKEN.as_bytes().to_vec()),
        ] {
            assert_eq!(
                a.purge(authorization.as_deref(), Some("all=1")),
                Err(PurgeRefusal::Unauthorized),
                "a bad credential was accepted"
            );
        }
    }

    #[test]
    fn purge_authorises_before_it_parses() {
        // Otherwise the error messages become a free description of which
        // parameters this build understands, for anyone with no credential.
        let a = admin_with_purge();
        assert_eq!(
            a.purge(None, Some("nonsense=1")),
            Err(PurgeRefusal::Unauthorized)
        );
    }

    #[test]
    fn purge_accepts_a_correct_token() {
        let a = admin_with_purge();
        let body = a.purge(Some(&bearer(TOKEN)), Some("all=1")).unwrap();
        assert!(body.contains(r#""purged":true"#), "{body}");
        assert!(body.contains(r#""scope":"all""#), "{body}");
        assert_balanced(&body);
    }

    #[test]
    fn purge_needs_to_be_told_what_to_purge() {
        let a = admin_with_purge();
        for query in [None, Some(""), Some("tag=")] {
            assert!(
                matches!(
                    a.purge(Some(&bearer(TOKEN)), query),
                    Err(PurgeRefusal::Malformed(_))
                ),
                "an empty purge was accepted: {query:?}"
            );
        }
    }

    #[test]
    fn secret_comparison_is_length_then_constant_time() {
        assert!(secret_eq(b"abc", b"abc"));
        assert!(!secret_eq(b"abc", b"abd"));
        assert!(!secret_eq(b"ab", b"abc"));
        assert!(!secret_eq(b"", b"a"));
        assert!(secret_eq(b"", b""));
    }

    #[test]
    fn purge_query_parsing_rejects_what_it_does_not_understand() {
        assert_eq!(
            parse_purge_query(Some("tag=a&tag=b")),
            Ok(PurgeRequest::Selective {
                tags: vec!["a".into(), "b".into()],
                paths: vec![]
            })
        );
        assert_eq!(
            parse_purge_query(Some("tag=a&tag=a")),
            Ok(PurgeRequest::Selective {
                tags: vec!["a".into()],
                paths: vec![]
            }),
            "repeats collapse"
        );
        assert_eq!(parse_purge_query(Some("all=1")), Ok(PurgeRequest::All));
        assert_eq!(parse_purge_query(Some("all=true")), Ok(PurgeRequest::All));

        // A typo that silently purged nothing would look like a working
        // invalidation until somebody checked.
        assert!(matches!(
            parse_purge_query(Some("tags=a")),
            Err(PurgeRefusal::Malformed(_))
        ));
        // Contradictory instructions are refused rather than guessed at.
        assert!(matches!(
            parse_purge_query(Some("all=1&tag=a")),
            Err(PurgeRefusal::Malformed(_))
        ));
        assert!(matches!(
            parse_purge_query(Some("all=1&path=/x")),
            Err(PurgeRefusal::Malformed(_))
        ));
    }

    #[test]
    fn purge_accepts_paths_beside_tags() {
        // One deploy hook that calls both `revalidateTag()` and
        // `revalidatePath()` should be one request, not two chances to
        // half-apply.
        assert_eq!(
            parse_purge_query(Some("tag=a&path=/products/iphone")),
            Ok(PurgeRequest::Selective {
                tags: vec!["a".into()],
                paths: vec!["/products/iphone".into()],
            })
        );
        assert_eq!(
            parse_purge_query(Some("path=/a&path=/b&path=/a")),
            Ok(PurgeRequest::Selective {
                tags: vec![],
                paths: vec!["/a".into(), "/b".into()],
            }),
            "repeats collapse"
        );
    }

    /// A relative path could never match a stored entry, since the stored
    /// value is the request path and always begins with `/`. Accepting one
    /// would be a purge that silently removed nothing.
    #[test]
    fn purge_refuses_a_relative_path() {
        for query in ["path=products", "path=", "path=./x"] {
            assert!(
                matches!(
                    parse_purge_query(Some(query)),
                    Err(PurgeRefusal::Malformed(_))
                ),
                "accepted {query}"
            );
        }
    }

    #[test]
    fn purge_bounds_how_many_paths_one_request_may_name() {
        let query = (0..MAX_PURGE_TAGS + 5)
            .map(|n| format!("path=/p/{n}"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(matches!(
            parse_purge_query(Some(&query)),
            Err(PurgeRefusal::Malformed(_))
        ));
    }

    #[test]
    fn purge_bounds_how_many_tags_one_request_may_name() {
        let query = (0..MAX_PURGE_TAGS + 5)
            .map(|n| format!("tag=t{n}"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(matches!(
            parse_purge_query(Some(&query)),
            Err(PurgeRefusal::Malformed(_))
        ));
    }

    #[test]
    fn purge_values_are_percent_decoded_once() {
        assert_eq!(
            parse_purge_query(Some("tag=sale%26clearance&path=%2Fproducts%2Fred%26blue")),
            Ok(PurgeRequest::Selective {
                tags: vec!["sale&clearance".into()],
                paths: vec!["/products/red&blue".into()]
            })
        );
        assert_eq!(
            parse_purge_query(Some("tag=a%252Cb")),
            Ok(PurgeRequest::Selective {
                tags: vec!["a%2Cb".into()],
                paths: vec![]
            }),
            "a literal percent must be decoded exactly once"
        );
    }

    #[test]
    fn purge_refuses_percent_decoded_values_that_are_not_utf8() {
        assert!(matches!(
            parse_purge_query(Some("tag=%ff")),
            Err(PurgeRefusal::Malformed(_))
        ));
    }

    #[test]
    fn the_status_document_says_whether_purge_is_enabled() {
        assert!(
            admin(false)
                .status_body()
                .contains(r#""purge_enabled":false"#)
        );
        let body = admin_with_purge().status_body();
        assert!(body.contains(r#""purge_enabled":true"#), "{body}");
        assert!(body.contains(r#""eviction":"clock""#), "{body}");
        assert!(
            body.contains(r#""tag_header":"x-tanod-cache-tags""#),
            "{body}"
        );
        assert_balanced(&body);
    }

    #[test]
    fn a_fresh_instance_is_ready() {
        assert_eq!(admin(false).readiness(), Ok(()));
    }

    #[test]
    fn draining_makes_readiness_fail_before_anything_stops_serving() {
        // The whole point: the balancer has to learn we are going away while
        // we are still able to answer.
        let a = admin(false);
        a.drain.begin("test");
        assert_eq!(a.readiness(), Err(NotReady::Draining));
        let (status, body) = a.readiness_body();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains(r#""ready":false"#), "{body}");
        assert!(body.contains(r#""reason":"draining""#), "{body}");
    }

    #[test]
    fn liveness_does_not_follow_readiness() {
        // A draining instance is still alive. Conflating the two makes an
        // orchestrator kill the process mid-drain, which is the opposite of
        // what draining is for.
        let a = admin(false);
        a.drain.begin("test");
        assert!(a.liveness_body().contains(r#""status":"live""#));
    }

    #[test]
    fn an_unhealthy_pool_only_fails_readiness_when_asked_for() {
        let lenient = admin(false);
        let strict = admin(true);
        for pool in [&lenient.upstreams, &strict.upstreams] {
            pool.set_healthy(0, false);
            pool.set_healthy(1, false);
        }
        // The default is deliberate: Tanod still serves a fully unhealthy
        // pool, so taking every replica out of rotation would turn a degraded
        // origin into a total outage at the edge too.
        assert_eq!(lenient.readiness(), Ok(()));
        assert_eq!(strict.readiness(), Err(NotReady::NoHealthyUpstream));

        strict.upstreams.set_healthy(1, true);
        assert_eq!(strict.readiness(), Ok(()));
    }

    #[test]
    fn the_status_document_is_parseable_json_with_the_operational_fields() {
        let a = admin(false);
        let body = a.status_body();
        assert!(body.starts_with('{') && body.ends_with('}'), "{body}");
        assert_balanced(&body);
        for expected in [
            r#""schema_version":1"#,
            r#""generation":7"#,
            r#""fingerprint":"#,
            r#""routes":1"#,
            r#""draining":false"#,
            r#""path":"/etc/tanod/tanod.yaml""#,
            r#""address":"127.0.0.1:3000""#,
            r#""healthy":true"#,
            r#""name":"global""#,
            r#""name":"products""#,
            r#""limit":10"#,
            r#""queue_timeout_ms":250"#,
            r#""bytes_used":0"#,
            r#""max_bytes":67108864"#,
            // Resilience state: the three priority tiers, per-backend breaker
            // and load figures, and the retry budget.
            r#""name":"tier:high""#,
            r#""name":"tier:normal""#,
            r#""name":"tier:low""#,
            r#""ejected":false"#,
            r#""in_flight":0"#,
            r#""latency_ewma_us":0"#,
            r#""window":{"ok":0,"failed":0}"#,
            r#""trips":0"#,
            r#""retry":{"enabled":false"#,
        ] {
            assert!(body.contains(expected), "missing {expected} in {body}");
        }
    }

    #[test]
    fn the_status_document_exposes_the_replica_group_budget() {
        let a = admin(false);
        let mut cfg = a.policy.load().config.clone();
        cfg.origin.concurrency.max = 4;
        cfg.capacity = Some(crate::config::schema::CapacityGroup {
            global_max: 9,
            replicas: 2,
        });
        a.policy.store(PolicySnapshot::build(cfg, 8).unwrap());
        let body = a.status_body();
        assert!(
            body.contains(
                r#""capacity":{"global_max":9,"replicas":2,"per_replica":4,"allocated":8}"#
            ),
            "{body}"
        );
    }

    /// A backend that passes its probe and fails its renders is the case
    /// passive observation exists for, so the two states have to be separately
    /// visible rather than collapsed into one field.
    #[test]
    fn the_status_document_reports_health_and_ejection_separately() {
        let breaker = crate::config::schema::Breaker {
            enabled: true,
            min_requests: 2,
            failure_percent: 50,
            max_ejected_percent: 50,
            ..Default::default()
        };
        let pool = UpstreamPool::new(
            &["127.0.0.1:3000".to_string(), "127.0.0.2:3000".to_string()],
            LoadBalancing::RoundRobin,
            &breaker,
        )
        .unwrap();
        pool.assume_healthy();
        pool.record_outcome(0, None, false);
        pool.record_outcome(0, None, false);

        let mut a = admin(false);
        a.upstreams = Arc::new(pool);
        let body = a.status_body();
        assert_balanced(&body);
        assert!(
            body.contains(r#""address":"127.0.0.1:3000","healthy":true,"ejected":true"#),
            "{body}"
        );
        assert!(
            body.contains(r#""address":"127.0.0.2:3000","healthy":true,"ejected":false"#),
            "{body}"
        );
        assert!(body.contains(r#""trips":1"#), "{body}");
    }

    #[test]
    fn backend_state_in_the_status_document_tracks_health_checking() {
        let a = admin(false);
        a.upstreams.set_healthy(0, false);
        let body = a.status_body();
        // Two backends, one of each state — a document that always said
        // `true` would be worse than not publishing the field.
        assert_eq!(body.matches(r#""healthy":false"#).count(), 1, "{body}");
        assert_eq!(body.matches(r#""healthy":true"#).count(), 1, "{body}");
    }

    #[test]
    fn the_status_document_reports_the_live_generation_not_the_startup_one() {
        let a = admin(false);
        let cfg = a.policy.load().config.clone();
        a.policy.store(PolicySnapshot::build(cfg, 8).unwrap());
        assert!(a.status_body().contains(r#""generation":8"#));
    }

    #[test]
    fn the_drain_reason_is_a_closed_set_not_free_text() {
        // It is rendered into JSON, so it must not be able to carry anything
        // a caller chose. `begin` takes a &'static str for that reason.
        let a = admin(false);
        a.drain.begin("sigterm");
        assert!(a.status_body().contains(r#""reason":"sigterm""#));
    }

    /// Braces and quotes balance, which is the cheap stand-in for a parser we
    /// deliberately do not depend on.
    fn assert_balanced(body: &str) {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for c in body.chars() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "unbalanced close in {body}");
        }
        assert!(!in_string, "unterminated string in {body}");
        assert_eq!(depth, 0, "unbalanced braces in {body}");
    }
}
