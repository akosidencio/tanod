//! Bounding how much origin work is in flight.
//!
//! This is the part of Tanod that keeps working when nothing is cacheable and
//! nothing can be collapsed, which is most of what a dynamic SSR app looks like.
//!
//! Order matters and is deliberate: reuse opportunities are exhausted *before*
//! admission, because a cache hit and a coalescing waiter consume no origin
//! capacity and should never be made to queue for it.

pub mod limiter;

use crate::classifier::RequestClass;
use crate::config::schema::{Priorities, Priority};
use limiter::{Limiter, Permit, ShedReason};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The outcome of asking for origin capacity.
#[derive(Debug)]
pub enum Admission {
    /// Hold these for exactly as long as the origin is working.
    Admitted(Permits),
    /// This class does not consume origin capacity. Static assets are served
    /// without rendering; streaming responses would hold a permit for the life
    /// of the connection and starve everything else.
    Exempt,
    Shed(ShedReason),
}

/// A request needs room globally, on its priority tier, *and* on its route.
/// All of them are released together when this drops.
#[derive(Debug)]
pub struct Permits {
    route: Option<Permit>,
    tier: Option<Permit>,
    global: Permit,
}

impl Permits {
    /// Collapse every permit into one value the request can own. The narrower
    /// permits ride along so that dropping this releases all of them together.
    pub fn into_inner(self) -> Permit {
        self.global
            .with_companions([self.tier, self.route].into_iter().flatten())
    }
}

pub struct AdmissionController {
    global: Arc<Limiter>,
    /// One ceiling per priority, each a share of the global one, indexed in
    /// [`Priority::ALL`] order. See [`Priorities`] for why reserved capacity is
    /// expressed as a ceiling rather than a reservation.
    ///
    /// Always consulted, even when every tier is allowed the whole global
    /// limit and so could never refuse anything. Skipping the uncontended case
    /// would save one atomic compare-exchange per request and buy a class of
    /// bug in return: a reload that lowers a tier would start enforcing a
    /// ceiling against in-flight requests that never took a permit on it, and
    /// the tier would sit over its limit until they drained.
    tiers: Vec<Arc<Limiter>>,
    /// Keyed by route id and deliberately outliving any one config
    /// generation — a limiter carries in-flight state that a policy swap
    /// must not discard.
    ///
    /// `parking_lot` rather than `std`: this lock is taken on the request
    /// path, and a poisoned `std` lock would leave every later request with
    /// nothing to do but panic again.
    routes: RwLock<HashMap<String, Arc<Limiter>>>,
}

/// A tier's ceiling: `global * percent / 100`, floored, but never zero.
///
/// A tier that rounded to nothing would refuse every request on it, which is a
/// total outage for those routes dressed up as a rounding error. Validation
/// refuses the combination up front; this is the floor that makes the runtime
/// behaviour survivable if it ever gets past.
fn tier_ceiling(global_max: usize, percent: u32) -> usize {
    (global_max.saturating_mul(percent as usize) / 100).max(1)
}

impl AdmissionController {
    pub fn new(
        global_max: usize,
        queue_max: usize,
        queue_timeout: Duration,
        priorities: &Priorities,
    ) -> Self {
        AdmissionController {
            global: Limiter::new("global", global_max, queue_max, queue_timeout),
            tiers: Priority::ALL
                .iter()
                .map(|priority| {
                    Limiter::new(
                        priority.limiter_name(),
                        tier_ceiling(global_max, priorities.percent_for(*priority)),
                        queue_max,
                        queue_timeout,
                    )
                })
                .collect(),
            routes: RwLock::new(HashMap::new()),
        }
    }

    pub fn global(&self) -> &Arc<Limiter> {
        &self.global
    }

    /// The tier ceilings, in [`Priority::ALL`] order, for the status document
    /// and the metrics gauges.
    pub fn tier_limiters(&self) -> &[Arc<Limiter>] {
        &self.tiers
    }

    pub fn tier_limiter(&self, priority: Priority) -> Option<&Arc<Limiter>> {
        Priority::ALL
            .iter()
            .position(|p| *p == priority)
            .and_then(|i| self.tiers.get(i))
    }

    /// Get or create the limiter for a route. Created on first use so that a
    /// newly added route does not need a restart.
    pub fn route_limiter(
        &self,
        id: &str,
        max: usize,
        queue_max: usize,
        queue_timeout: Duration,
    ) -> Arc<Limiter> {
        if let Some(l) = self.routes.read().get(id) {
            return l.clone();
        }
        let mut w = self.routes.write();
        w.entry(id.to_string())
            .or_insert_with(|| Limiter::new(id, max, queue_max, queue_timeout))
            .clone()
    }

    /// Every route limiter currently registered, for the admin status
    /// document. A snapshot of the `Arc`s rather than a borrow, so the read
    /// lock is held for the length of a clone and not for the length of
    /// rendering a JSON document.
    pub fn route_limiters(&self) -> Vec<Arc<Limiter>> {
        let mut limiters: Vec<Arc<Limiter>> = self.routes.read().values().cloned().collect();
        // Stable order: a status document whose fields move between scrapes
        // is one nobody can diff.
        limiters.sort_by(|a, b| a.name().cmp(b.name()));
        limiters
    }

    /// Apply new limits from a reloaded config, in place.
    ///
    /// Limiters are resized rather than replaced, and one that has disappeared
    /// from the config is dropped from the registry but stays alive until its
    /// last in-flight permit returns.
    pub fn apply_limits(
        &self,
        global_max: usize,
        global_queue_max: usize,
        global_queue_timeout: Duration,
        priorities: &Priorities,
        routes: &[(String, usize, usize, Duration)],
    ) {
        self.global.resize(global_max);
        self.global
            .set_queue(global_queue_max, global_queue_timeout);
        // Tier ceilings track the global one, so raising `concurrency.max`
        // raises every tier's share of it without the shares being restated.
        for (index, priority) in Priority::ALL.iter().enumerate() {
            if let Some(tier) = self.tiers.get(index) {
                tier.resize(tier_ceiling(global_max, priorities.percent_for(*priority)));
                tier.set_queue(global_queue_max, global_queue_timeout);
            }
        }
        let mut w = self.routes.write();
        for (id, max, q_max, q_timeout) in routes {
            match w.get(id) {
                Some(existing) => {
                    existing.resize(*max);
                    existing.set_queue(*q_max, *q_timeout);
                }
                None => {
                    w.insert(
                        id.clone(),
                        Limiter::new(id.clone(), *max, *q_max, *q_timeout),
                    );
                }
            }
        }
        let keep: Vec<String> = routes.iter().map(|(id, ..)| id.clone()).collect();
        w.retain(|id, _| keep.contains(id));
    }

    /// Ask for capacity.
    ///
    /// Narrowest ceiling first: the route, then the priority tier, then the
    /// global limit. Failing early costs the wider limiters nothing, and a
    /// slot taken on the way to a refusal is handed straight back when the
    /// local permit drops.
    ///
    /// Every acquisition shares one deadline, so a request cannot spend each
    /// limiter's queue timeout in turn and wait three times as long as
    /// configured.
    ///
    /// `weight` is how many units of each ceiling this request occupies. See
    /// [`crate::config::schema::Route::weight`].
    pub async fn admit(
        &self,
        class: RequestClass,
        route: Option<&Arc<Limiter>>,
        priority: Priority,
        weight: u32,
    ) -> Admission {
        if !class.consumes_origin_permit() {
            return Admission::Exempt;
        }

        let tier = self.tier_limiter(priority);
        let budget = std::iter::once(self.global.queue_timeout())
            .chain(route.map(|limiter| limiter.queue_timeout()))
            .chain(tier.map(|limiter| limiter.queue_timeout()))
            .filter(|duration| !duration.is_zero())
            .min();
        let deadline = budget.map(|duration| Instant::now() + duration);

        let route_permit = match route {
            Some(r) => match r.acquire(deadline, weight).await {
                Ok(p) => Some(p),
                Err(reason) => return Admission::Shed(reason),
            },
            None => None,
        };

        // Dropping `route_permit` on the way out of either arm below hands the
        // route slot straight back, which is what keeps a refusal further down
        // from leaking capacity on the limiters already passed.
        let tier_permit = match tier {
            Some(t) => match t.acquire(deadline, weight).await {
                Ok(p) => Some(p),
                Err(reason) => return Admission::Shed(reason),
            },
            None => None,
        };

        match self.global.acquire(deadline, weight).await {
            Ok(g) => Admission::Admitted(Permits {
                route: route_permit,
                tier: tier_permit,
                global: g,
            }),
            Err(reason) => Admission::Shed(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn controller(global: usize) -> AdmissionController {
        AdmissionController::new(global, 0, Duration::ZERO, &Priorities::default())
    }

    /// Admit at the default priority and weight, which is what every test
    /// that is not about priorities or weights wants.
    async fn admit(
        c: &AdmissionController,
        class: RequestClass,
        route: Option<&Arc<Limiter>>,
    ) -> Admission {
        c.admit(class, route, Priority::Normal, 1).await
    }

    #[tokio::test]
    async fn static_and_streaming_bypass_the_permit_entirely() {
        let c = controller(1);
        let _held = admit(&c, RequestClass::PublicDocument, None).await;
        // The one global permit is gone, yet these still pass.
        assert!(matches!(
            admit(&c, RequestClass::Static, None).await,
            Admission::Exempt
        ));
        assert!(matches!(
            admit(&c, RequestClass::Streaming, None).await,
            Admission::Exempt
        ));
    }

    #[tokio::test]
    async fn admission_requires_both_route_and_global_capacity() {
        let c = controller(10);
        let route = c.route_limiter("search", 1, 0, Duration::ZERO);
        let _a = admit(&c, RequestClass::PublicDynamic, Some(&route)).await;
        // Global has room; the route does not.
        assert!(matches!(
            admit(&c, RequestClass::PublicDynamic, Some(&route)).await,
            Admission::Shed(_)
        ));
    }

    #[tokio::test]
    async fn a_route_slot_is_returned_when_the_global_limit_refuses() {
        let c = controller(1);
        let route = c.route_limiter("r", 5, 0, Duration::ZERO);
        let _global_hog = admit(&c, RequestClass::PublicDocument, None).await;

        assert!(matches!(
            admit(&c, RequestClass::PublicDocument, Some(&route)).await,
            Admission::Shed(_)
        ));
        // The route limiter must not have leaked the slot it briefly held.
        assert_eq!(
            route.available(),
            5,
            "route permit was not released after global shed"
        );
    }

    #[tokio::test]
    async fn origin_concurrency_never_exceeds_the_configured_maximum() {
        // The acceptance criterion from the spec, as an executable test.
        const MAX: usize = 10;
        const ARRIVALS: usize = 200;

        let c = Arc::new(AdmissionController::new(
            MAX,
            500,
            Duration::from_secs(5),
            &Priorities::default(),
        ));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..ARRIVALS {
            let (c, in_flight, peak) = (c.clone(), in_flight.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                if let Admission::Admitted(p) = admit(&c, RequestClass::PublicDocument, None).await
                {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    drop(p);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= MAX,
            "peak {} exceeded {MAX}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0, "a permit leaked");
    }

    #[tokio::test]
    async fn reload_resizes_in_place_and_keeps_the_same_limiter() {
        let c = controller(10);
        let before = c.route_limiter("products", 100, 0, Duration::ZERO);
        c.apply_limits(
            20,
            25,
            Duration::from_secs(2),
            &Priorities::default(),
            &[("products".into(), 50, 10, Duration::from_secs(1))],
        );
        let after = c.route_limiter("products", 100, 0, Duration::ZERO);

        assert!(
            Arc::ptr_eq(&before, &after),
            "reload replaced the limiter instead of resizing it"
        );
        assert_eq!(after.limit(), 50);
        assert_eq!(c.global().limit(), 20);
        assert_eq!(c.global().queue_timeout(), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_route_removed_from_config_leaves_the_registry() {
        let c = controller(10);
        c.route_limiter("old", 5, 0, Duration::ZERO);
        c.apply_limits(
            10,
            0,
            Duration::ZERO,
            &Priorities::default(),
            &[("new".into(), 5, 0, Duration::ZERO)],
        );
        assert!(c.routes.read().get("old").is_none());
        assert!(c.routes.read().get("new").is_some());
    }

    // ----------------------------------------- priorities and reserved room

    fn tiered(global: usize, high: u32, normal: u32, low: u32) -> AdmissionController {
        AdmissionController::new(global, 0, Duration::ZERO, &Priorities { high, normal, low })
    }

    #[tokio::test]
    async fn the_default_priorities_let_every_tier_use_the_whole_ceiling() {
        let c = controller(4);
        let mut held = Vec::new();
        for priority in [Priority::Low, Priority::Low, Priority::Low, Priority::Low] {
            match c
                .admit(RequestClass::PublicDocument, None, priority, 1)
                .await
            {
                Admission::Admitted(p) => held.push(p),
                other => panic!("uniform priorities refused a request: {other:?}"),
            }
        }
        assert_eq!(held.len(), 4);
    }

    /// The property the whole feature exists for: however much low-priority
    /// traffic arrives, it cannot occupy the room kept for everything else.
    #[tokio::test]
    async fn low_priority_traffic_cannot_consume_the_reserved_capacity() {
        let c = tiered(10, 100, 100, 50);
        let mut held = Vec::new();
        for _ in 0..20 {
            if let Admission::Admitted(p) = c
                .admit(RequestClass::PublicDocument, None, Priority::Low, 1)
                .await
            {
                held.push(p);
            }
        }
        assert_eq!(held.len(), 5, "low priority took more than its half");

        // And the reserved half is genuinely still there for other work.
        for _ in 0..5 {
            assert!(matches!(
                c.admit(RequestClass::PublicDocument, None, Priority::High, 1)
                    .await,
                Admission::Admitted(_)
            ));
        }
    }

    #[tokio::test]
    async fn a_tier_ceiling_never_exceeds_the_global_one() {
        // Every tier is allowed everything, so the global limit is the only
        // thing left to stop them.
        let c = tiered(3, 100, 100, 100);
        let mut held = Vec::new();
        for priority in [Priority::High, Priority::Normal, Priority::Low] {
            for _ in 0..3 {
                if let Admission::Admitted(p) = c
                    .admit(RequestClass::PublicDocument, None, priority, 1)
                    .await
                {
                    held.push(p);
                }
            }
        }
        assert_eq!(
            held.len(),
            3,
            "the tiers between them beat the global limit"
        );
    }

    #[tokio::test]
    async fn a_tier_slot_is_returned_when_the_global_limit_refuses() {
        let c = tiered(1, 100, 100, 100);
        let _hog = c
            .admit(RequestClass::PublicDocument, None, Priority::High, 1)
            .await;
        assert!(matches!(
            c.admit(RequestClass::PublicDocument, None, Priority::Low, 1)
                .await,
            Admission::Shed(_)
        ));
        let low = c.tier_limiter(Priority::Low).unwrap();
        assert_eq!(
            low.available(),
            low.limit(),
            "the tier permit leaked when the global limit shed the request"
        );
    }

    #[tokio::test]
    async fn reload_resizes_the_tiers_along_with_the_global_ceiling() {
        let c = tiered(10, 100, 100, 50);
        assert_eq!(c.tier_limiter(Priority::Low).unwrap().limit(), 5);
        c.apply_limits(
            100,
            0,
            Duration::ZERO,
            &Priorities {
                high: 100,
                normal: 90,
                low: 20,
            },
            &[],
        );
        assert_eq!(c.tier_limiter(Priority::High).unwrap().limit(), 100);
        assert_eq!(c.tier_limiter(Priority::Normal).unwrap().limit(), 90);
        assert_eq!(c.tier_limiter(Priority::Low).unwrap().limit(), 20);
    }

    #[tokio::test]
    async fn a_tier_share_that_rounds_to_nothing_still_admits_one_request() {
        // Validation refuses this combination; if it ever gets through, a
        // ceiling of zero would be a silent outage for that tier.
        let c = tiered(5, 100, 100, 10);
        assert_eq!(c.tier_limiter(Priority::Low).unwrap().limit(), 1);
        assert!(matches!(
            c.admit(RequestClass::PublicDocument, None, Priority::Low, 1)
                .await,
            Admission::Admitted(_)
        ));
    }

    // ------------------------------------------------- weighted admission

    #[tokio::test]
    async fn a_heavier_route_consumes_more_of_the_ceiling() {
        let c = controller(6);
        let _heavy = c
            .admit(RequestClass::PublicDocument, None, Priority::Normal, 4)
            .await;
        assert_eq!(c.global().available(), 2);
        assert!(matches!(
            c.admit(RequestClass::PublicDocument, None, Priority::Normal, 3)
                .await,
            Admission::Shed(_)
        ));
        assert!(matches!(
            c.admit(RequestClass::PublicDocument, None, Priority::Normal, 2)
                .await,
            Admission::Admitted(_)
        ));
    }

    #[tokio::test]
    async fn weight_is_charged_to_the_route_and_the_tier_as_well() {
        let c = tiered(20, 100, 100, 100);
        let route = c.route_limiter("search", 6, 0, Duration::ZERO);
        let _first = c
            .admit(RequestClass::PublicDynamic, Some(&route), Priority::Low, 3)
            .await;
        assert_eq!(route.available(), 3);
        assert_eq!(c.tier_limiter(Priority::Low).unwrap().available(), 17);
        assert_eq!(c.global().available(), 17);
    }

    #[tokio::test]
    async fn every_permit_a_weighted_request_took_is_returned_together() {
        let c = tiered(20, 100, 100, 100);
        let route = c.route_limiter("search", 6, 0, Duration::ZERO);
        let admission = c
            .admit(RequestClass::PublicDynamic, Some(&route), Priority::Low, 3)
            .await;
        let Admission::Admitted(permits) = admission else {
            panic!("not admitted");
        };
        let held = permits.into_inner();
        assert_eq!(route.available(), 3);
        drop(held);
        assert_eq!(route.available(), 6);
        assert_eq!(c.tier_limiter(Priority::Low).unwrap().available(), 20);
        assert_eq!(c.global().available(), 20);
    }

    #[tokio::test]
    async fn the_shorter_global_queue_deadline_wins_over_a_route_deadline() {
        let c = AdmissionController::new(1, 10, Duration::from_millis(30), &Priorities::default());
        let route = c.route_limiter("slow-route", 10, 10, Duration::from_secs(1));
        let _held = admit(&c, RequestClass::PublicDocument, None).await;
        let started = Instant::now();

        assert!(matches!(
            admit(&c, RequestClass::PublicDocument, Some(&route)).await,
            Admission::Shed(ShedReason::QueueTimeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}
