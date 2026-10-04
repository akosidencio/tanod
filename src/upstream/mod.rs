//! Where a request goes, and whether that backend is worth sending it to.
//!
//! Three signals decide, in this order:
//!
//! 1. **Active health checks** ([`health`]) — a probe on a configured path.
//!    Cheap, periodic, and blind to everything it does not ask about.
//! 2. **Passive failure observation** ([`breaker`]) — the outcome of the real
//!    requests Tanod is already sending. This is what notices an origin that
//!    answers `/healthz` fine and fails every render.
//! 3. **Live load** — in-flight work and observed latency per backend, which
//!    is what [`crate::config::schema::LoadBalancing::LeastLoaded`] selects on.
//!
//! Every one of them is advisory. A pool with nothing left to prefer still
//! serves, because refusing to pick turns a degraded origin into a guaranteed
//! outage, and `stale_if_error` exists precisely for that window.

pub mod breaker;
pub mod health;
pub mod resolver;
pub mod retry;
pub mod window;

use std::net::{SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;

use crate::config::schema::{Breaker as BreakerConfig, LoadBalancing};
use breaker::{Breaker, BreakerAllowance, BreakerState, ProbeToken};

/// One configured upstream: the name from `origin.upstreams` and every
/// address it currently resolves to.
///
/// A name, not an address, because the platforms Tanod runs on hand out one
/// DNS name for a set of instances — App Platform's internal service name,
/// a Kubernetes headless Service — and grow, shrink and replace that set
/// without telling anyone. Resolving once and keeping the first address, as
/// Tanod used to, pinned every origin connection to one instance: on the
/// staging storefront one instance ran at 100% CPU while its twin idled, and a
/// replaced instance would have been unreachable until Tanod restarted.
/// [`resolver`] re-resolves every `origin.resolve_interval`.
#[derive(Debug, Clone)]
pub struct Backend {
    pub id: usize,
    pub address: String,
    sockets: Arc<ArcSwap<Vec<SocketAddr>>>,
    rotation: Arc<AtomicUsize>,
}

impl Backend {
    fn new(id: usize, address: &str) -> Result<Self, String> {
        let sockets = resolve(address)?;
        Ok(Backend {
            id,
            address: address.to_string(),
            sockets: Arc::new(ArcSwap::from_pointee(sockets)),
            rotation: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Every address this upstream currently resolves to, in a stable order.
    pub fn sockets(&self) -> Arc<Vec<SocketAddr>> {
        self.sockets.load_full()
    }

    /// The address one origin connection should use.
    ///
    /// With a `key` — the path, under `hash_by_path` — the same path keeps
    /// reaching the same instance, which is the point of that strategy: the
    /// instance's own render cache stays warm. Without one, instances take
    /// turns. The set never goes empty: [`Backend::refresh`] keeps the last
    /// good answer rather than store none.
    pub fn socket_for(&self, key: Option<&str>) -> SocketAddr {
        let sockets = self.sockets.load();
        let len = sockets.len().max(1);
        let index = match key {
            Some(key) => usize::try_from(fnv1a(key.as_bytes()) % len as u64).unwrap_or(0),
            None => self.rotation.fetch_add(1, Ordering::Relaxed) % len,
        };
        sockets[index.min(sockets.len() - 1)]
    }

    /// Resolves the name again. Returns whether the set of addresses changed.
    ///
    /// A failed or empty lookup keeps the previous addresses: a DNS blip must
    /// not take a working origin out of rotation, and the health checker and
    /// breakers already handle instances that really are gone.
    pub fn refresh(&self) -> Result<bool, String> {
        let fresh = resolve(&self.address)?;
        if **self.sockets.load() == fresh {
            return Ok(false);
        }
        self.sockets.store(Arc::new(fresh));
        Ok(true)
    }
}

/// Every address `address` resolves to, sorted and without duplicates so that
/// hashing a path onto them is stable between refreshes.
fn resolve(address: &str) -> Result<Vec<SocketAddr>, String> {
    let mut sockets: Vec<SocketAddr> = address
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve upstream `{address}`: {error}"))?
        .collect();
    sockets.sort();
    sockets.dedup();
    if sockets.is_empty() {
        return Err(format!("upstream `{address}` resolved to no addresses"));
    }
    Ok(sockets)
}

/// A routing decision and, when applicable, the recovery-probe identity that
/// must travel with this exact origin attempt.
#[derive(Clone, Copy)]
pub struct SelectedBackend<'a> {
    pub backend: &'a Backend,
    probe: Option<ProbeToken>,
}

impl std::ops::Deref for SelectedBackend<'_> {
    type Target = Backend;

    fn deref(&self) -> &Self::Target {
        self.backend
    }
}

/// Everything Tanod has learned about one backend since it started.
struct BackendState {
    /// Set false by health checking. Starts false: a configured probe has not
    /// passed yet at startup, and strict readiness must not describe an
    /// unprobed backend as healthy.
    healthy: AtomicBool,
    /// Origin requests currently outstanding to this backend. Incremented at
    /// selection and released when the origin finishes, not when the client
    /// finishes reading — see [`InFlightGuard`].
    in_flight: AtomicUsize,
    /// Exponentially weighted mean time to first byte, in microseconds. Zero
    /// means nothing has been observed yet.
    ewma_micros: AtomicU64,
    breaker: Breaker,
}

pub struct UpstreamPool {
    backends: Vec<Backend>,
    state: Vec<BackendState>,
    strategy: LoadBalancing,
    cursor: AtomicUsize,
    /// The most backends that may be ejected by their breakers at once.
    ///
    /// Zero when breaking is disabled, which makes every breaker check inert.
    max_ejected: usize,
    /// Monotonic origin for the millisecond clock the breakers run on. Held
    /// here rather than read from each breaker so every backend in one
    /// selection is judged against the same instant.
    epoch: Instant,
}

impl UpstreamPool {
    pub fn new(
        addresses: &[String],
        strategy: LoadBalancing,
        breaker: &BreakerConfig,
    ) -> Result<Self, String> {
        let backends = addresses
            .iter()
            .enumerate()
            .map(|(id, address)| Backend::new(id, address))
            .collect::<Result<Vec<_>, String>>()?;
        let state = backends
            .iter()
            .map(|_| BackendState {
                healthy: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                ewma_micros: AtomicU64::new(0),
                breaker: Breaker::new(breaker),
            })
            .collect();
        // `len * percent / 100`, floored. With breaking off this is zero and
        // the first ejection immediately exceeds it, so `select` never
        // consults a breaker at all.
        let max_ejected = if breaker.enabled {
            backends
                .len()
                .saturating_mul(breaker.max_ejected_percent as usize)
                / 100
        } else {
            0
        };
        Ok(UpstreamPool {
            backends,
            state,
            strategy,
            cursor: AtomicUsize::new(0),
            max_ejected,
            epoch: Instant::now(),
        })
    }

    pub fn len(&self) -> usize {
        self.backends.len()
    }

    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    pub fn strategy(&self) -> LoadBalancing {
        self.strategy
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Milliseconds since this pool was built. The clock every breaker
    /// decision and observation is stamped with.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn state(&self, id: usize) -> Option<&BackendState> {
        self.state.get(id)
    }

    pub fn set_healthy(&self, id: usize, healthy: bool) {
        if let Some(state) = self.state(id) {
            state.healthy.store(healthy, Ordering::Relaxed);
        }
    }

    /// Mark every backend available when active health checking is disabled.
    /// In that mode there is no probe whose result could move the pool out of
    /// an initial unknown state.
    pub fn assume_healthy(&self) {
        for backend in &self.backends {
            self.set_healthy(backend.id, true);
        }
    }

    /// Is this backend currently passing its health check?
    ///
    /// Public because the admin status document publishes per-backend state,
    /// which is the first thing anyone asks during an origin incident.
    pub fn is_healthy(&self, id: usize) -> bool {
        self.state(id)
            .is_some_and(|s| s.healthy.load(Ordering::Relaxed))
    }

    /// How many backends are passing. Zero does not stop Tanod serving —
    /// see [`UpstreamPool::select`] — but it is what an operator wants
    /// readiness to be able to report.
    pub fn healthy_count(&self) -> usize {
        self.backends
            .iter()
            .filter(|b| self.is_healthy(b.id))
            .count()
    }

    /// Has this backend's breaker tripped?
    pub fn breaker_state(&self, id: usize) -> BreakerState {
        self.state(id)
            .map_or(BreakerState::Closed, |s| s.breaker.state())
    }

    /// `(successes, failures)` this backend has recorded inside the breaker's
    /// window, and how many times it has been ejected.
    pub fn breaker_counts(&self, id: usize) -> (u64, u64, u64) {
        match self.state(id) {
            Some(s) => {
                let (ok, fail) = s.breaker.counts(self.now_ms());
                (ok, fail, s.breaker.trips())
            }
            None => (0, 0, 0),
        }
    }

    /// How many backends are currently ejected by their breakers.
    pub fn ejected_count(&self) -> usize {
        self.state
            .iter()
            .filter(|s| s.breaker.state() == BreakerState::Open)
            .count()
    }

    pub fn in_flight(&self, id: usize) -> usize {
        self.state(id)
            .map_or(0, |s| s.in_flight.load(Ordering::Relaxed))
    }

    /// The exponentially weighted mean time to first byte, in microseconds.
    /// Zero means nothing has been observed yet.
    pub fn ewma_micros(&self, id: usize) -> u64 {
        self.state(id)
            .map_or(0, |s| s.ewma_micros.load(Ordering::Relaxed))
    }

    /// Record how one origin request ended.
    ///
    /// This is the passive observation the breaker runs on, and it is called
    /// for every attempt — a connect failure, a proxy error, and an ordinary
    /// response whose status says the origin could not do its job.
    pub fn record_outcome(&self, id: usize, probe: Option<ProbeToken>, ok: bool) {
        if let Some(state) = self.state(id) {
            match probe {
                Some(token) => state.breaker.record_probe(self.now_ms(), token, ok),
                None => state.breaker.record(self.now_ms(), ok),
            }
        }
    }

    /// Fold one time-to-first-byte observation into a backend's mean.
    ///
    /// Time to first byte rather than total response time: it is what reflects
    /// the origin's own queueing, and it does not make a backend that served a
    /// large body look slow.
    pub fn observe_latency(&self, id: usize, micros: u64) {
        let Some(state) = self.state(id) else {
            return;
        };
        let _ = state
            .ewma_micros
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
                Some(match previous {
                    // Nothing observed yet: the first sample *is* the mean,
                    // rather than one eighth of it. Starting at zero and
                    // easing in would make a backend that has served one slow
                    // request look like the fastest in the pool.
                    0 => micros,
                    // alpha = 1/8, in integers. `previous / 8` never exceeds
                    // `previous`, so the subtraction cannot underflow.
                    p => (p - p / 8).saturating_add(micros / 8),
                })
            });
    }

    /// Claim a slot on a backend for as long as the origin is working on it.
    pub fn enter(self: &Arc<Self>, id: usize) -> InFlightGuard {
        self.enter_attempt(id, None)
    }

    pub fn enter_selected(self: &Arc<Self>, selected: SelectedBackend<'_>) -> InFlightGuard {
        self.enter_attempt(selected.id, selected.probe)
    }

    fn enter_attempt(self: &Arc<Self>, id: usize, probe: Option<ProbeToken>) -> InFlightGuard {
        if let Some(state) = self.state(id) {
            state.in_flight.fetch_add(1, Ordering::Relaxed);
        }
        InFlightGuard {
            pool: self.clone(),
            id,
            probe,
        }
    }

    /// Pick a backend for this path.
    pub fn select(&self, path: &str) -> Option<SelectedBackend<'_>> {
        self.select_at(path, self.now_ms())
    }

    /// [`UpstreamPool::select`], against a caller-supplied clock.
    ///
    /// The layering is deliberate. Health is the outer filter because a
    /// backend that failed its probe is the one signal an operator configured
    /// explicitly. Breakers are consulted inside that set, and only up to
    /// `max_ejected_percent` of the pool — past that, every backend is failing
    /// and "failing" has stopped being a reason to prefer one over another.
    /// Both filters collapse to the full pool rather than to nothing.
    pub fn select_at(&self, path: &str, now_ms: u64) -> Option<SelectedBackend<'_>> {
        if self.backends.is_empty() {
            return None;
        }
        let healthy: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| self.is_healthy(b.id))
            .collect();
        // Nothing healthy: serve anyway rather than converting a degraded
        // origin into a guaranteed outage.
        if healthy.is_empty() {
            return self
                .pick(&self.backends.iter().collect::<Vec<_>>(), path)
                .map(|backend| SelectedBackend {
                    backend,
                    probe: None,
                });
        }

        if self.max_ejected == 0 {
            return self.pick(&healthy, path).map(|backend| SelectedBackend {
                backend,
                probe: None,
            });
        }
        let ejected: Vec<&Backend> = healthy
            .iter()
            .filter(|b| self.breaker_state(b.id) == BreakerState::Open)
            .copied()
            .collect();
        // The outlier-ejection cap. When an origin-wide dependency fails,
        // every backend fails, every breaker trips, and a proxy that honoured
        // all of them would have nowhere left to send anything.
        // Spend a recovery probe if one is due. This is checked before normal
        // selection, and it has to be: an ejected backend that is never picked
        // never produces the observation that would close its breaker, so a
        // pool with one good backend left would eject the rest permanently.
        // The cost is one request per `open_for` per ejected backend.
        for backend in &ejected {
            if let Some(state) = self.state(backend.id) {
                match state.breaker.allowance(now_ms) {
                    BreakerAllowance::Probe(token) => {
                        return Some(SelectedBackend {
                            backend,
                            probe: Some(token),
                        });
                    }
                    // It closed concurrently with the ejected snapshot. It is
                    // safe to use immediately as ordinary traffic.
                    BreakerAllowance::Normal => {
                        return Some(SelectedBackend {
                            backend,
                            probe: None,
                        });
                    }
                    BreakerAllowance::Denied => {}
                }
            }
        }

        if ejected.len() > self.max_ejected {
            return self.pick(&healthy, path).map(|backend| SelectedBackend {
                backend,
                // This is cap fallback traffic, not the one request that won a
                // recovery token above. Its result cannot decide the breaker.
                probe: None,
            });
        }

        let available: Vec<&Backend> = healthy
            .iter()
            .filter(|b| self.breaker_state(b.id) == BreakerState::Closed)
            .copied()
            .collect();
        if available.is_empty() {
            return self.pick(&healthy, path).map(|backend| SelectedBackend {
                backend,
                probe: None,
            });
        }
        self.pick(&available, path).map(|backend| SelectedBackend {
            backend,
            probe: None,
        })
    }

    /// Apply the configured strategy to a set of candidates.
    fn pick<'a>(&self, pool: &[&'a Backend], path: &str) -> Option<&'a Backend> {
        let len = NonZeroUsize::new(pool.len())?;
        match self.strategy {
            LoadBalancing::RoundRobin => pool
                .get(self.cursor.fetch_add(1, Ordering::Relaxed) % len)
                .copied(),
            // Sending a given path to a consistent backend also warms the
            // origin's own render cache and JIT state, which is free origin
            // work avoided on top of anything Tanod does.
            LoadBalancing::HashByPath => {
                // The remainder is smaller than `len`, so it always fits back
                // into a `usize`; `unwrap_or` is unreachable, not a fallback.
                let index = usize::try_from(fnv1a(path.as_bytes()) % len.get() as u64).unwrap_or(0);
                pool.get(index).copied()
            }
            LoadBalancing::LeastLoaded => self.least_loaded(pool, len),
        }
    }

    /// The backend with the least outstanding work.
    ///
    /// A full scan rather than the usual two-random-choices. Power-of-two
    /// choices exists to stop many selectors converging on one apparently-idle
    /// backend before any of them has updated its load; here the in-flight
    /// count is incremented at selection time, so the next selector already
    /// sees it. That leaves the scan's O(n), and an SSR origin's `n` is single
    /// digits — the pool is a set of render processes, not a fleet.
    fn least_loaded<'a>(&self, pool: &[&'a Backend], len: NonZeroUsize) -> Option<&'a Backend> {
        // Start the scan at a rotating offset so that backends which score
        // identically — the common case on an idle pool, where every score is
        // 1 — take turns instead of piling onto the lowest-numbered one.
        let offset = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut best: Option<(&'a Backend, u64)> = None;
        for step in 0..len.get() {
            let Some(backend) = pool.get(offset.wrapping_add(step) % len) else {
                continue;
            };
            let score = self.score(backend.id);
            if best.is_none_or(|(_, previous)| score < previous) {
                best = Some((backend, score));
            }
        }
        best.map(|(backend, _)| backend)
    }

    /// Outstanding work against observed latency.
    ///
    /// The product, not either half. In-flight count alone treats a backend
    /// that is answering in 5ms and one that is answering in 5s as equally
    /// busy at the same depth; latency alone ignores the queue that has
    /// already formed. Multiplying gives the quantity that actually matters,
    /// which is how long a request arriving now would expect to wait.
    fn score(&self, id: usize) -> u64 {
        let in_flight = u64::try_from(self.in_flight(id)).unwrap_or(u64::MAX);
        // `+ 1` so an idle backend still scores by its latency rather than
        // zeroing out and tying with every other idle backend.
        // `max(1)` so a backend with no observation yet is ranked purely on
        // depth instead of scoring zero and attracting everything.
        in_flight
            .saturating_add(1)
            .saturating_mul(self.ewma_micros(id).max(1))
    }
}

/// One origin request's occupancy of a backend.
///
/// Released when the *origin* finishes, not when the client finishes reading —
/// the same distinction the admission permit makes, and for the same reason: a
/// slow reader must not make a backend look loaded. Dropping is the release,
/// so every path out of a request returns the slot, including the ones that
/// end in an error.
pub struct InFlightGuard {
    pool: Arc<UpstreamPool>,
    id: usize,
    probe: Option<ProbeToken>,
}

impl InFlightGuard {
    pub fn backend_id(&self) -> usize {
        self.id
    }

    pub(crate) fn probe_token(&self) -> Option<ProbeToken> {
        self.probe
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(state) = self.pool.state(self.id) {
            state.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::units::Dur;
    use std::time::Duration;

    fn breaker_off() -> BreakerConfig {
        BreakerConfig::default()
    }

    fn breaker_on(min_requests: u32, max_ejected_percent: u32) -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            window: Dur(Duration::from_millis(1000)),
            min_requests,
            failure_percent: 50,
            open_for: Dur(Duration::from_millis(500)),
            max_ejected_percent,
        }
    }

    fn addresses(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("127.0.0.{i}:3000")).collect()
    }

    fn pool_of(n: usize, strategy: LoadBalancing, breaker: &BreakerConfig) -> Arc<UpstreamPool> {
        let pool = UpstreamPool::new(&addresses(n), strategy, breaker).unwrap();
        pool.assume_healthy();
        Arc::new(pool)
    }

    fn pool(strategy: LoadBalancing) -> Arc<UpstreamPool> {
        pool_of(3, strategy, &breaker_off())
    }

    #[test]
    fn a_new_pool_is_unknown_until_a_probe_or_explicit_assumption() {
        let p = UpstreamPool::new(
            &["127.0.0.1:3000".to_string()],
            LoadBalancing::RoundRobin,
            &breaker_off(),
        )
        .unwrap();
        assert_eq!(p.healthy_count(), 0);
        p.assume_healthy();
        assert_eq!(p.healthy_count(), 1);
    }

    #[test]
    fn round_robin_cycles_through_every_backend() {
        let p = pool(LoadBalancing::RoundRobin);
        let picks: Vec<&str> = (0..6)
            .map(|_| p.select("/x").unwrap().backend.address.as_str())
            .collect();
        assert_eq!(
            picks,
            [
                "127.0.0.1:3000",
                "127.0.0.2:3000",
                "127.0.0.3:3000",
                "127.0.0.1:3000",
                "127.0.0.2:3000",
                "127.0.0.3:3000",
            ]
        );
    }

    #[test]
    fn hash_by_path_is_stable_for_one_path() {
        let p = pool(LoadBalancing::HashByPath);
        let first = p.select("/products/iphone").unwrap().id;
        for _ in 0..20 {
            assert_eq!(p.select("/products/iphone").unwrap().id, first);
        }
    }

    #[test]
    fn hash_by_path_spreads_distinct_paths() {
        let p = pool(LoadBalancing::HashByPath);
        let ids: std::collections::HashSet<usize> = (0..60)
            .map(|i| p.select(&format!("/p/{i}")).unwrap().id)
            .collect();
        assert!(ids.len() > 1, "every path landed on one backend");
    }

    #[test]
    fn unhealthy_backends_are_skipped() {
        let p = pool(LoadBalancing::RoundRobin);
        p.set_healthy(0, false);
        for _ in 0..10 {
            assert_ne!(p.select("/x").unwrap().id, 0);
        }
    }

    #[test]
    fn a_fully_unhealthy_pool_still_serves() {
        // Refusing to pick would turn a degraded origin into a hard outage.
        let p = pool(LoadBalancing::RoundRobin);
        for id in 0..3 {
            p.set_healthy(id, false);
        }
        assert!(p.select("/x").is_some());
    }

    // ------------------------------------------------------- least loaded

    #[test]
    fn least_loaded_prefers_the_backend_with_fewer_requests_outstanding() {
        let p = pool_of(3, LoadBalancing::LeastLoaded, &breaker_off());
        for id in 0..3 {
            p.observe_latency(id, 1_000);
        }
        let _a = p.enter(0);
        let _b = p.enter(0);
        let _c = p.enter(1);
        assert_eq!(p.select("/x").unwrap().id, 2);
    }

    #[test]
    fn least_loaded_prefers_the_faster_backend_at_equal_depth() {
        let p = pool_of(3, LoadBalancing::LeastLoaded, &breaker_off());
        p.observe_latency(0, 50_000);
        p.observe_latency(1, 5_000);
        p.observe_latency(2, 500_000);
        for _ in 0..10 {
            assert_eq!(p.select("/x").unwrap().id, 1);
        }
    }

    /// The failure a plain in-flight count cannot see: a backend that is
    /// answering, so it is never unhealthy, and answering slowly, so it should
    /// be getting less work rather than its even third.
    #[test]
    fn least_loaded_routes_around_a_backend_that_is_up_but_slow() {
        let p = pool_of(2, LoadBalancing::LeastLoaded, &breaker_off());
        p.observe_latency(0, 2_000); // 2ms
        p.observe_latency(1, 2_000_000); // 2s
        let mut held = Vec::new();
        for _ in 0..8 {
            let picked = p.select("/x").unwrap().id;
            held.push(p.enter(picked));
        }
        let fast = held.iter().filter(|g| g.backend_id() == 0).count();
        assert!(
            fast >= 7,
            "the slow backend took {} of 8 requests",
            8 - fast
        );
    }

    #[test]
    fn least_loaded_rotates_between_backends_that_score_the_same() {
        let p = pool_of(3, LoadBalancing::LeastLoaded, &breaker_off());
        let ids: std::collections::HashSet<usize> =
            (0..9).map(|_| p.select("/x").unwrap().id).collect();
        assert_eq!(ids.len(), 3, "an idle pool collapsed onto one backend");
    }

    #[test]
    fn an_in_flight_slot_is_returned_when_its_guard_drops() {
        let p = pool_of(2, LoadBalancing::LeastLoaded, &breaker_off());
        {
            let _g = p.enter(0);
            assert_eq!(p.in_flight(0), 1);
        }
        assert_eq!(p.in_flight(0), 0);
    }

    #[test]
    fn the_first_latency_observation_is_the_mean_rather_than_an_eighth_of_it() {
        let p = pool_of(1, LoadBalancing::LeastLoaded, &breaker_off());
        p.observe_latency(0, 80_000);
        assert_eq!(p.ewma_micros(0), 80_000);
        // Then it eases: one 8ms sample moves an 80ms mean by an eighth.
        p.observe_latency(0, 8_000);
        assert_eq!(p.ewma_micros(0), 80_000 - 10_000 + 1_000);
    }

    // ----------------------------------------------------------- breakers

    #[test]
    fn a_backend_failing_real_traffic_is_ejected_even_while_it_probes_healthy() {
        let p = pool_of(3, LoadBalancing::RoundRobin, &breaker_on(4, 50));
        for _ in 0..4 {
            p.record_outcome(1, None, false);
        }
        assert_eq!(p.breaker_state(1), BreakerState::Open);
        assert!(p.is_healthy(1), "health checking still says it is fine");

        let now = p.now_ms();
        for _ in 0..20 {
            assert_ne!(p.select_at("/x", now).unwrap().id, 1);
        }
    }

    #[test]
    fn breakers_are_inert_until_they_are_turned_on() {
        let p = pool_of(3, LoadBalancing::RoundRobin, &breaker_off());
        for _ in 0..100 {
            p.record_outcome(1, None, false);
        }
        assert_eq!(p.breaker_state(1), BreakerState::Closed);
        let ids: std::collections::HashSet<usize> =
            (0..9).map(|_| p.select("/x").unwrap().id).collect();
        assert_eq!(ids.len(), 3);
    }

    /// The cap that keeps a breaker from causing the outage it exists to
    /// contain: when an origin-wide dependency dies, every backend fails and
    /// honouring every breaker would leave nowhere to route.
    #[test]
    fn ejecting_more_than_the_cap_falls_back_to_health_based_routing() {
        let p = pool_of(4, LoadBalancing::RoundRobin, &breaker_on(4, 50));
        for id in 0..4 {
            for _ in 0..4 {
                p.record_outcome(id, None, false);
            }
        }
        assert_eq!(p.ejected_count(), 4);

        let now = p.now_ms();
        let ids: std::collections::HashSet<usize> = (0..12)
            .map(|_| p.select_at("/x", now).unwrap().id)
            .collect();
        assert_eq!(
            ids.len(),
            4,
            "past the ejection cap every backend must be usable again"
        );
    }

    #[test]
    fn exactly_the_cap_may_be_ejected() {
        let p = pool_of(4, LoadBalancing::RoundRobin, &breaker_on(4, 50));
        for id in [0, 1] {
            for _ in 0..4 {
                p.record_outcome(id, None, false);
            }
        }
        assert_eq!(p.ejected_count(), 2);
        let now = p.now_ms();
        let ids: std::collections::HashSet<usize> = (0..12)
            .map(|_| p.select_at("/x", now).unwrap().id)
            .collect();
        assert_eq!(ids, [2, 3].into_iter().collect());
    }

    /// Without this an ejected backend never receives the request that would
    /// prove it recovered, so the first blip ejects it forever.
    #[test]
    fn an_ejected_backend_gets_one_probe_per_period_and_can_come_back() {
        let p = pool_of(4, LoadBalancing::RoundRobin, &breaker_on(4, 50));
        for _ in 0..4 {
            p.record_outcome(1, None, false);
        }
        let opened_at = p.now_ms();

        // Inside the open period nothing reaches it.
        for _ in 0..20 {
            assert_ne!(p.select_at("/x", opened_at).unwrap().id, 1);
        }

        // Once it expires exactly one request does.
        let probe_at = opened_at + 500;
        let probe = p.select_at("/x", probe_at).unwrap();
        assert_eq!(probe.id, 1);
        for _ in 0..20 {
            assert_ne!(p.select_at("/x", probe_at).unwrap().id, 1);
        }

        // And a good result puts it back in rotation.
        p.record_outcome(1, probe.probe, true);
        assert_eq!(p.breaker_state(1), BreakerState::Closed);
        let ids: std::collections::HashSet<usize> = (0..12)
            .map(|_| p.select_at("/x", probe_at).unwrap().id)
            .collect();
        assert!(ids.contains(&1));
    }

    #[test]
    fn an_unhealthy_backend_is_skipped_before_its_breaker_is_consulted() {
        let p = pool_of(3, LoadBalancing::RoundRobin, &breaker_on(4, 50));
        p.set_healthy(2, false);
        for _ in 0..4 {
            p.record_outcome(0, None, false);
        }
        let now = p.now_ms();
        for _ in 0..20 {
            assert_eq!(
                p.select_at("/x", now).unwrap().id,
                1,
                "one healthy backend with a closed breaker is the only choice"
            );
        }
    }

    /// A backend whose name stands for several instances, as App Platform's
    /// internal service name does when a component runs more than one.
    fn multi_instance(addresses: &[&str]) -> Backend {
        let sockets: Vec<SocketAddr> = addresses.iter().map(|a| a.parse().unwrap()).collect();
        Backend {
            id: 0,
            address: "origin:3000".to_string(),
            sockets: Arc::new(ArcSwap::from_pointee(sockets)),
            rotation: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn instances_behind_one_name_all_receive_connections() {
        // Regression: the first resolved address was kept and every other
        // instance idled — the staging storefront ran one instance at 100% CPU
        // while its twin sat at 0%.
        let backend = multi_instance(&["10.0.0.1:3000", "10.0.0.2:3000", "10.0.0.3:3000"]);
        let used: std::collections::HashSet<SocketAddr> =
            (0..9).map(|_| backend.socket_for(None)).collect();

        assert_eq!(used.len(), 3);
    }

    #[test]
    fn hash_by_path_keeps_a_path_on_one_instance() {
        let backend = multi_instance(&["10.0.0.1:3000", "10.0.0.2:3000"]);
        let first = backend.socket_for(Some("/books/noli-me-tangere"));

        for _ in 0..5 {
            assert_eq!(backend.socket_for(Some("/books/noli-me-tangere")), first);
        }
    }

    #[test]
    fn a_failed_lookup_keeps_the_previous_addresses() {
        let backend = multi_instance(&["10.0.0.1:3000", "10.0.0.2:3000"]);
        let unresolvable = Backend {
            address: "tanod-test.invalid:3000".to_string(),
            ..backend.clone()
        };

        assert!(unresolvable.refresh().is_err());
        assert_eq!(
            backend.sockets().len(),
            2,
            "a DNS blip must not empty the pool"
        );
    }

    #[test]
    fn a_lookup_returning_the_same_addresses_reports_no_change() {
        let backend = Backend::new(0, "127.0.0.1:3000").unwrap();

        assert_eq!(backend.refresh(), Ok(false));
        assert_eq!(
            *backend.sockets(),
            vec!["127.0.0.1:3000".parse::<SocketAddr>().unwrap()]
        );
    }
}
