//! A concurrency limiter whose ceiling can move while requests are in flight.
//!
//! Reload is why this exists. Policy is immutable and swapped wholesale, but a
//! limiter is *stateful*: its outstanding permits belong to it. Swapping in a
//! fresh `Semaphore` on reload would let the old permits and the new ones both
//! be valid at once, transiently doubling admitted concurrency — on precisely
//! the config change most likely to be made during an incident, which is
//! raising a limit.
//!
//! Occupancy is counted separately from the semaphore. `limit - available`
//! is only occupancy while the ceiling stands still: after a shrink, permits
//! still out with in-flight requests are owed to the semaphore rather than
//! missing from it, and that subtraction reports the new ceiling instead of
//! the work actually running. [`Limiter::in_flight`] is the count of units
//! held by live [`Permit`]s, and is legitimately above [`Limiter::limit`]
//! while a lowered ceiling drains.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// A timeout in whole milliseconds, saturating.
///
/// `Duration::as_millis` is a `u128`. Clamping keeps an absurd configured
/// timeout absurd instead of wrapping it into a small one, and keeps the value
/// below the range where `Instant::now() + timeout` would overflow.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    /// The queue was already at its bound. Refused immediately.
    QueueFull,
    /// Waited in the queue and the deadline passed.
    QueueTimeout,
}

impl ShedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ShedReason::QueueFull => "queue_full",
            ShedReason::QueueTimeout => "queue_timeout",
        }
    }

    /// The admission metric's `decision` label. Static rather than formatted
    /// from [`as_str`](Self::as_str), because it is written on every shed.
    pub fn metric_label(self) -> &'static str {
        match self {
            ShedReason::QueueFull => "shed_queue_full",
            ShedReason::QueueTimeout => "shed_queue_timeout",
        }
    }
}

/// Origin work in flight. Releasing it is what lets the next request in, so it
/// is held for exactly as long as the origin is busy.
///
/// A permit may be worth more than one unit — see the `weight` argument to
/// [`Limiter::acquire`] — because not every request is the same load on an
/// origin.
#[derive(Debug)]
pub struct Permit {
    inner: Option<OwnedSemaphorePermit>,
    limiter: Arc<Limiter>,
    /// Permits released at the same moment as this one. A request needs
    /// capacity on its tier, its route and the global ceiling, and they must
    /// be given back together or the limiters drift out of step.
    companions: Vec<Permit>,
}

impl Permit {
    /// Bind other permits to this one's lifetime. Dropping this releases all
    /// of them, in one operation as far as any caller can observe.
    #[must_use]
    pub fn with_companions(mut self, others: impl IntoIterator<Item = Permit>) -> Permit {
        self.companions.extend(others);
        self
    }

    /// How many units of the ceiling this permit occupies.
    pub fn weight(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, OwnedSemaphorePermit::num_permits)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(mut permit) = self.inner.take() {
            // Leave the occupancy count before the units become acquirable
            // again. The other order lets a waiter take and count them while
            // they are still counted here, briefly reporting more work than
            // the semaphore has ever admitted.
            //
            // Relaxed is enough: this is the only decrement for units added by
            // exactly one increment in `Limiter::permit`, and both are
            // read-modify-writes on one atomic, so they can neither be lost nor
            // reordered against each other. Moving the permit to another task
            // already synchronised the two. No other memory is published
            // through the counter.
            self.limiter
                .in_flight
                .fetch_sub(permit.num_permits(), Ordering::Relaxed);
            // Resize and return form one semaphore transaction. Without this
            // guard, a shrink can forget the currently available permits, a
            // group of in-flight permits can return, and only then can the
            // shrink record its debt. Queued requests can steal every permit
            // returned in that window and keep concurrency near the old
            // ceiling for another generation of work.
            let mut debt = self.limiter.debt.lock();
            let units = permit.num_permits();
            let absorbed = (*debt).min(units);
            *debt -= absorbed;
            if absorbed == units {
                // Absorb the whole return directly. Handing it to the
                // semaphore first lets an already-queued waiter steal it.
                permit.forget();
            } else if absorbed > 0 {
                // A weighted permit can settle part of the debt and still owe
                // the ceiling the rest. Split so each half goes where it
                // belongs; a partial split that somehow failed would leave the
                // debt paid and the permits returned, so the debt is restored
                // rather than lost.
                match permit.split(absorbed) {
                    Some(settled) => settled.forget(),
                    None => *debt += absorbed,
                }
                // Make the remainder visible while the guard is still held. A
                // following resize therefore observes and collects it.
                drop(permit);
            } else {
                drop(permit);
            }
        }
    }
}

#[derive(Debug)]
pub struct Limiter {
    name: String,
    sem: Arc<Semaphore>,
    /// The configured ceiling, which is not the same as the semaphore's
    /// available count.
    limit: AtomicUsize,
    /// Permits a shrink still owes but could not take, because they were
    /// out with in-flight requests at the time.
    /// Also serialises ceiling changes with permit returns. The critical
    /// sections contain no await and only semaphore/integer operations.
    debt: Mutex<usize>,
    /// Units held by live permits. Counted at the permit rather than derived
    /// from the semaphore, so a resize can never rewrite it: see the module
    /// documentation.
    in_flight: AtomicUsize,
    queue_depth: AtomicUsize,
    queue_max: AtomicUsize,
    /// Milliseconds, not a `Duration`, because it has to live in an atomic.
    /// `u64` rather than `usize`: `Duration::as_millis` is a `u128`, and
    /// narrowing it to a pointer-sized integer silently rewrites a large
    /// configured timeout into a different one on a 32-bit target.
    queue_timeout_ms: AtomicU64,

    pub admitted: AtomicUsize,
    pub queued: AtomicUsize,
    pub shed: AtomicUsize,
}

impl Limiter {
    pub fn new(
        name: impl Into<String>,
        limit: usize,
        queue_max: usize,
        queue_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Limiter {
            name: name.into(),
            sem: Arc::new(Semaphore::new(limit)),
            limit: AtomicUsize::new(limit),
            debt: Mutex::new(0),
            in_flight: AtomicUsize::new(0),
            queue_depth: AtomicUsize::new(0),
            queue_max: AtomicUsize::new(queue_max),
            queue_timeout_ms: AtomicU64::new(millis(queue_timeout)),
            admitted: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            shed: AtomicUsize::new(0),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Units the semaphore would hand out right now. Not occupancy: after a
    /// shrink this is zero while the debt is collected, however much work is
    /// running. Use [`in_flight`](Self::in_flight) for that.
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// Units of origin work currently held by permits from this limiter,
    /// weighted by each request's weight.
    ///
    /// Above [`limit`](Self::limit) while a lowered ceiling drains, which is
    /// the point: it reports the work that is running, not the work that would
    /// be allowed to start.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Wrap a semaphore permit and count its units.
    ///
    /// The only constructor of a live [`Permit`], and synchronous: no await
    /// separates the acquisition from the increment, so a cancelled caller
    /// either never got the semaphore permit or owns a `Permit` whose drop
    /// undoes the count.
    fn permit(self: &Arc<Self>, inner: OwnedSemaphorePermit) -> Permit {
        self.in_flight
            .fetch_add(inner.num_permits(), Ordering::Relaxed);
        self.admitted.fetch_add(1, Ordering::Relaxed);
        Permit {
            inner: Some(inner),
            limiter: self.clone(),
            companions: Vec::new(),
        }
    }

    /// Move the ceiling without disturbing in-flight work.
    ///
    /// Resizes are serialised with each other and with permit returns so a
    /// downward change has no interval in which returned permits can escape
    /// its debt accounting.
    pub fn resize(self: &Arc<Self>, new_limit: usize) {
        let mut debt = self.debt.lock();
        let old = self.limit.swap(new_limit, Ordering::SeqCst);
        if new_limit > old {
            // Growing also cancels any outstanding shrink.
            let grant = new_limit - old;
            // One read-modify-write keeps the debt transition explicit. The
            // resize lock also prevents a return from observing a partial
            // grow while the matching permits are added.
            let cancelled = (*debt).min(grant);
            *debt -= cancelled;
            let grant = grant - cancelled;
            if grant > 0 {
                self.sem.add_permits(grant);
            }
        } else if new_limit < old {
            let owed = old - new_limit;
            // Permit returns cannot interleave these two operations because
            // they take the same guard. Whatever cannot be forgotten here is
            // therefore genuinely in flight and becomes exact shrink debt.
            let forgotten = self.sem.forget_permits(owed);
            if forgotten < owed {
                // The rest are out with in-flight requests; collect them as
                // they come back rather than over-admitting in the meantime.
                *debt += owed - forgotten;
            }
        }
    }

    pub fn set_queue(&self, max: usize, timeout: Duration) {
        self.queue_max.store(max, Ordering::Relaxed);
        self.queue_timeout_ms
            .store(millis(timeout), Ordering::Relaxed);
    }

    /// The configured queue bound. Zero means no queue at all: a request that
    /// cannot be admitted immediately is shed.
    pub fn queue_max(&self) -> usize {
        self.queue_max.load(Ordering::Relaxed)
    }

    pub fn queue_timeout(&self) -> Duration {
        Duration::from_millis(self.queue_timeout_ms.load(Ordering::Relaxed))
    }

    /// Take a permit, or say why not.
    ///
    /// `deadline` bounds the *total* wait, so a request crossing several
    /// limiters cannot spend each one's queue timeout in turn.
    ///
    /// `weight` is how many units of the ceiling this one request occupies.
    /// One is the ordinary case and says every request is the same load; a
    /// route that fans out to three services can be given a weight so that a
    /// ceiling of 50 means fifty units of origin work rather than fifty
    /// requests of wildly different cost. A weight larger than the ceiling can
    /// never be satisfied, which is why configuration refuses one.
    pub async fn acquire(
        self: &Arc<Self>,
        deadline: Option<Instant>,
        weight: u32,
    ) -> Result<Permit, ShedReason> {
        // A weight of zero would take nothing and bound nothing. Treated as
        // one rather than rejected: the caller has already been through
        // validation, and admitting an unbounded request is the worse failure.
        let weight = weight.max(1);
        if let Ok(p) = self.sem.clone().try_acquire_many_owned(weight) {
            return Ok(self.permit(p));
        }

        let queue_max = self.queue_max.load(Ordering::Relaxed);
        if queue_max == 0 {
            self.shed.fetch_add(1, Ordering::Relaxed);
            return Err(ShedReason::QueueFull);
        }

        // Reserve a queue slot before waiting. An unbounded queue turns an
        // origin overload into a proxy overload and defers the failure rather
        // than preventing it.
        let depth = self.queue_depth.fetch_add(1, Ordering::SeqCst);
        if depth >= queue_max {
            self.queue_depth.fetch_sub(1, Ordering::SeqCst);
            self.shed.fetch_add(1, Ordering::Relaxed);
            return Err(ShedReason::QueueFull);
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        let queue_slot = QueueSlot {
            limiter: self.clone(),
        };

        let wait = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => self.queue_timeout(),
        };

        let result = tokio::time::timeout(wait, self.sem.clone().acquire_many_owned(weight)).await;
        drop(queue_slot);

        match result {
            Ok(Ok(p)) => Ok(self.permit(p)),
            // The semaphore is closed only at shutdown.
            Ok(Err(_)) => {
                self.shed.fetch_add(1, Ordering::Relaxed);
                Err(ShedReason::QueueFull)
            }
            Err(_) => {
                self.shed.fetch_add(1, Ordering::Relaxed);
                Err(ShedReason::QueueTimeout)
            }
        }
    }
}

struct QueueSlot {
    limiter: Arc<Limiter>,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.limiter.queue_depth.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shed_metric_labels_match_the_reason() {
        for reason in [ShedReason::QueueFull, ShedReason::QueueTimeout] {
            assert_eq!(reason.metric_label(), format!("shed_{}", reason.as_str()));
        }
    }

    fn lim(limit: usize, q: usize, t_ms: u64) -> Arc<Limiter> {
        Limiter::new("test", limit, q, Duration::from_millis(t_ms))
    }

    #[tokio::test]
    async fn admits_up_to_the_limit_and_no_further() {
        let l = lim(2, 0, 0);
        let _a = l.acquire(None, 1).await.unwrap();
        let _b = l.acquire(None, 1).await.unwrap();
        assert_eq!(l.acquire(None, 1).await.unwrap_err(), ShedReason::QueueFull);
    }

    #[tokio::test]
    async fn a_released_permit_admits_the_next_request() {
        let l = lim(1, 0, 0);
        let a = l.acquire(None, 1).await.unwrap();
        assert!(l.acquire(None, 1).await.is_err());
        drop(a);
        assert!(l.acquire(None, 1).await.is_ok());
    }

    #[tokio::test]
    async fn queue_is_bounded_and_refuses_immediately_when_full() {
        let l = lim(1, 1, 5_000);
        let _held = l.acquire(None, 1).await.unwrap();
        let l2 = l.clone();
        // One waiter occupies the single queue slot.
        let waiter = tokio::spawn(async move { l2.acquire(None, 1).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(l.queue_depth(), 1);
        // The next arrival is refused rather than queued.
        assert_eq!(l.acquire(None, 1).await.unwrap_err(), ShedReason::QueueFull);
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(
            l.queue_depth(),
            0,
            "cancelled waiters must return their queue slot"
        );
    }

    #[tokio::test]
    async fn queue_deadline_sheds_rather_than_waiting_forever() {
        let l = lim(1, 10, 50);
        let _held = l.acquire(None, 1).await.unwrap();
        assert_eq!(
            l.acquire(None, 1).await.unwrap_err(),
            ShedReason::QueueTimeout
        );
    }

    #[tokio::test]
    async fn growing_the_limit_admits_more_immediately() {
        let l = lim(1, 0, 0);
        let _a = l.acquire(None, 1).await.unwrap();
        assert!(l.acquire(None, 1).await.is_err());
        l.resize(3);
        assert!(l.acquire(None, 1).await.is_ok());
        assert!(l.acquire(None, 1).await.is_ok());
        assert_eq!(l.limit(), 3);
    }

    #[tokio::test]
    async fn shrinking_never_over_admits_while_requests_are_in_flight() {
        // The reload hazard: 4 permits out, ceiling drops to 1. Returning
        // permits must be absorbed, not handed to new arrivals.
        let l = lim(4, 0, 0);
        let a = l.acquire(None, 1).await.unwrap();
        let b = l.acquire(None, 1).await.unwrap();
        let c = l.acquire(None, 1).await.unwrap();
        let d = l.acquire(None, 1).await.unwrap();

        l.resize(1);
        assert_eq!(l.limit(), 1);

        drop(a);
        assert!(
            l.acquire(None, 1).await.is_err(),
            "still 3 in flight against a ceiling of 1"
        );
        drop(b);
        assert!(
            l.acquire(None, 1).await.is_err(),
            "still 2 in flight against a ceiling of 1"
        );
        drop(c);
        assert!(
            l.acquire(None, 1).await.is_err(),
            "still 1 in flight against a ceiling of 1"
        );
        drop(d);
        assert!(
            l.acquire(None, 1).await.is_ok(),
            "now empty, one permit is correct"
        );
    }

    #[tokio::test]
    async fn growing_cancels_a_pending_shrink() {
        let l = lim(4, 0, 0);
        let a = l.acquire(None, 1).await.unwrap();
        let _b = l.acquire(None, 1).await.unwrap();
        l.resize(1); // owes 2
        l.resize(4); // operator changed their mind
        drop(a);
        assert!(l.acquire(None, 1).await.is_ok());
        assert_eq!(l.limit(), 4);
    }

    /// A grow smaller than the outstanding debt cancels part of it and leaves
    /// the rest owed. The whole debt must not fall on the floor: every permit
    /// still owed has to be absorbed on its way back, or the limiter settles
    /// above the ceiling it was just given.
    #[tokio::test]
    async fn growing_by_less_than_the_debt_leaves_the_remainder_owed() {
        let l = lim(5, 0, 0);
        let a = l.acquire(None, 1).await.unwrap();
        let b = l.acquire(None, 1).await.unwrap();
        let c = l.acquire(None, 1).await.unwrap();
        let d = l.acquire(None, 1).await.unwrap();
        let _e = l.acquire(None, 1).await.unwrap();

        l.resize(1); // owes 4; nothing is available to forget outright
        l.resize(2); // cancels one, three still owed
        assert_eq!(l.limit(), 2);

        for (n, permit) in [a, b, c].into_iter().enumerate() {
            drop(permit);
            assert!(
                l.acquire(None, 1).await.is_err(),
                "return {} of 3 must be absorbed against the remaining debt",
                n + 1
            );
        }

        drop(d);
        assert!(
            l.acquire(None, 1).await.is_ok(),
            "the debt is settled, so this return is the ceiling's own permit"
        );
    }

    // ------------------------------------------------- weighted admission

    #[tokio::test]
    async fn a_weighted_request_occupies_its_whole_weight() {
        let l = lim(6, 0, 0);
        let _heavy = l.acquire(None, 4).await.unwrap();
        assert_eq!(l.available(), 2);
        let _light = l.acquire(None, 2).await.unwrap();
        assert_eq!(l.acquire(None, 1).await.unwrap_err(), ShedReason::QueueFull);
    }

    #[tokio::test]
    async fn releasing_a_weighted_permit_returns_every_unit() {
        let l = lim(6, 0, 0);
        let heavy = l.acquire(None, 5).await.unwrap();
        assert_eq!(heavy.weight(), 5);
        drop(heavy);
        assert_eq!(l.available(), 6);
    }

    /// A ceiling of 6 admits six cheap requests or two expensive ones, and the
    /// point of weighting is that those are the same amount of origin work.
    #[tokio::test]
    async fn a_ceiling_bounds_units_of_work_rather_than_requests() {
        let l = lim(6, 0, 0);
        let mut held = Vec::new();
        while let Ok(p) = l.acquire(None, 3).await {
            held.push(p);
        }
        assert_eq!(held.len(), 2);
        assert_eq!(l.available(), 0);
    }

    /// The reload hazard, with weights: a shrink's debt is measured in units,
    /// so a returning heavy permit can settle part of it and hand back the
    /// rest.
    #[tokio::test]
    async fn a_weighted_return_settles_shrink_debt_unit_by_unit() {
        let l = lim(8, 0, 0);
        let heavy = l.acquire(None, 6).await.unwrap();
        assert_eq!(l.available(), 2);

        l.resize(4); // 4 owed; 2 are available to forget outright, 2 are out
        assert_eq!(l.available(), 0);

        drop(heavy); // returns 6: 2 settle the remaining debt, 4 come back
        assert_eq!(
            l.available(),
            4,
            "the ceiling settled somewhere other than its new limit"
        );
        assert!(l.acquire(None, 4).await.is_ok());
    }

    #[tokio::test]
    async fn a_zero_weight_still_takes_a_slot() {
        // Validation refuses a zero weight, so this is the belt to that
        // braces: an unbounded request is a worse outcome than a rejected one.
        let l = lim(1, 0, 0);
        let _held = l.acquire(None, 0).await.unwrap();
        assert_eq!(l.available(), 0);
    }

    #[tokio::test]
    async fn shrinking_absorbs_a_returned_permit_before_a_waiter_can_take_it() {
        let l = lim(2, 1, 5_000);
        let a = l.acquire(None, 1).await.unwrap();
        let b = l.acquire(None, 1).await.unwrap();
        let waiting_limiter = l.clone();
        let mut waiter = tokio::spawn(async move { waiting_limiter.acquire(None, 1).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        l.resize(1);
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut waiter)
                .await
                .is_err(),
            "the shrink debt was handed to a queued request"
        );

        drop(b);
        assert!(waiter.await.unwrap().is_ok());
    }

    // ------------------------------------------------ in-flight accounting
    //
    // Every test here asserts occupancy through `in_flight()` and, where the
    // two disagree, shows what `limit - available` would have said. That
    // subtraction is what the status document and the gauges used to report.

    fn derived(l: &Limiter) -> usize {
        l.limit().saturating_sub(l.available())
    }

    #[tokio::test]
    async fn in_flight_counts_weighted_units_not_requests() {
        let l = lim(10, 0, 0);
        let a = l.acquire(None, 4).await.unwrap();
        let b = l.acquire(None, 2).await.unwrap();
        let c = l.acquire(None, 1).await.unwrap();
        assert_eq!(l.in_flight(), 7);
        drop(a);
        assert_eq!(l.in_flight(), 3);
        drop((b, c));
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), 10);
    }

    /// The reported bug: four units running, ceiling lowered to one. The
    /// derived figure said one; four requests were still at the origin.
    #[tokio::test]
    async fn shrinking_a_full_limiter_keeps_reporting_the_work_still_running() {
        let l = lim(4, 0, 0);
        let held: Vec<_> = hold(&l, 4, 1).await;
        l.resize(1);

        assert_eq!(l.limit(), 1);
        assert_eq!(l.available(), 0);
        assert_eq!(l.in_flight(), 4, "a shrink rewrote historical occupancy");
        assert_eq!(derived(&l), 1, "the old derivation, for contrast");

        // Draining: occupancy falls one release at a time and only reaches
        // the new ceiling once three of the four have finished.
        let mut held = held.into_iter();
        for expected in [3, 2, 1, 0] {
            drop(held.next());
            assert_eq!(l.in_flight(), expected);
        }
        assert_eq!(l.available(), 1);
    }

    #[tokio::test]
    async fn shrinking_below_occupancy_reports_more_than_the_limit_until_drained() {
        let l = lim(6, 0, 0);
        let a = l.acquire(None, 3).await.unwrap();
        let _b = l.acquire(None, 2).await.unwrap();
        l.resize(2); // 1 forgotten outright, 3 owed

        assert!(l.in_flight() > l.limit());
        assert_eq!(l.in_flight(), 5);
        assert!(l.acquire(None, 1).await.is_err(), "admitted past the debt");

        drop(a); // all three units settle debt
        assert_eq!(l.in_flight(), 2);
        assert_eq!(l.available(), 0, "the ceiling is exactly full");
    }

    /// A weighted permit that settles only part of the debt: the counter
    /// drops by the whole weight, the semaphore gets back only the rest.
    #[tokio::test]
    async fn a_partial_debt_settlement_releases_the_whole_weight_from_in_flight() {
        let l = lim(8, 0, 0);
        let heavy = l.acquire(None, 6).await.unwrap();
        let light = l.acquire(None, 2).await.unwrap();
        l.resize(3); // owes 5, none available to forget

        drop(heavy); // 5 settle the debt, 1 returns
        assert_eq!(l.in_flight(), 2);
        assert_eq!(l.available(), 1);

        let next = l.acquire(None, 1).await.unwrap();
        assert_eq!(l.in_flight(), 3);
        assert_eq!(l.in_flight(), l.limit());
        drop((light, next));
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), 3);
    }

    #[tokio::test]
    async fn growing_after_a_shrink_counts_new_work_on_top_of_the_old() {
        let l = lim(4, 0, 0);
        let held = hold(&l, 4, 1).await;
        l.resize(1);
        l.resize(6); // cancels the 3 owed, grants 2 more

        assert_eq!(l.in_flight(), 4);
        assert_eq!(l.available(), 2);
        let extra = l.acquire(None, 2).await.unwrap();
        assert_eq!(l.in_flight(), 6);
        drop(held);
        drop(extra);
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), 6);
    }

    #[tokio::test]
    async fn repeated_grow_and_shrink_cycles_settle_exactly() {
        let l = lim(5, 0, 0);
        let mut held = Vec::new();
        for (round, size) in [2usize, 9, 1, 7, 3, 3, 12, 4].into_iter().enumerate() {
            // Take what fits, release about half, move the ceiling.
            while let Ok(p) = l.acquire(None, [1, 2][round % 2]).await {
                held.push(p);
            }
            let keep = held.len() / 2;
            held.truncate(keep);
            l.resize(size);
            let counted: usize = held.iter().map(Permit::weight).sum();
            assert_eq!(l.in_flight(), counted, "round {round}");
        }
        drop(held);
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), l.limit(), "debt or permits leaked");
    }

    #[tokio::test]
    async fn a_queue_timeout_takes_no_units() {
        let l = lim(1, 4, 20);
        let held = l.acquire(None, 1).await.unwrap();
        assert_eq!(
            l.acquire(None, 1).await.unwrap_err(),
            ShedReason::QueueTimeout
        );
        assert_eq!(l.in_flight(), 1);
        assert_eq!(l.queue_depth(), 0);
        drop(held);
        assert_eq!(l.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_waiter_cancelled_in_the_queue_takes_no_units() {
        let l = lim(1, 4, 5_000);
        let held = l.acquire(None, 1).await.unwrap();
        let waiting = l.clone();
        let waiter = tokio::spawn(async move { waiting.acquire(None, 1).await });
        while l.queue_depth() == 0 {
            tokio::task::yield_now().await;
        }
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(l.in_flight(), 1);
        assert_eq!(l.queue_depth(), 0);
        drop(held);
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), 1);
    }

    /// A request cancelled after admission — a client disconnect — drops its
    /// permit with the task, and that is the release.
    #[tokio::test]
    async fn a_request_cancelled_while_admitted_releases_its_units() {
        let l = lim(4, 0, 0);
        let holding = l.clone();
        let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _permit = holding.acquire(None, 3).await.unwrap();
            let _ = admitted_tx.send(());
            std::future::pending::<()>().await;
        });
        admitted_rx.await.unwrap();
        assert_eq!(l.in_flight(), 3);
        task.abort();
        let _ = task.await;
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.available(), 4);
    }

    /// A waiter woken by a release counts its units once it holds them, and
    /// the releaser's units have left the count by then.
    #[tokio::test]
    async fn a_queued_waiter_is_counted_once_admitted() {
        let l = lim(2, 4, 5_000);
        let held = l.acquire(None, 2).await.unwrap();
        let waiting = l.clone();
        let waiter = tokio::spawn(async move { waiting.acquire(None, 2).await });
        while l.queue_depth() == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(l.in_flight(), 2, "a queued request is not in flight");
        drop(held);
        let permit = waiter.await.unwrap().unwrap();
        assert_eq!(l.in_flight(), 2);
        drop(permit);
        assert_eq!(l.in_flight(), 0);
    }

    /// Releases and resizes racing on real threads. Each worker owns a
    /// current-thread runtime because only `rt` is enabled for this crate; the
    /// operations under test are synchronous anyway.
    #[test]
    fn concurrent_releases_and_resizes_never_lose_or_invent_units() {
        use std::sync::Barrier;
        const WORKERS: usize = 4;
        const ROUNDS: usize = 2_000;
        const SIZES: [usize; 5] = [16, 3, 9, 1, 12];
        let biggest = *SIZES.iter().max().unwrap();

        let l = lim(SIZES[0], 0, 0);
        let start = Arc::new(Barrier::new(WORKERS + 1));
        let mut threads = Vec::new();
        for worker in 0..WORKERS {
            let (l, start) = (l.clone(), start.clone());
            threads.push(std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap();
                start.wait();
                rt.block_on(async {
                    let weight = [1, 2, 3][worker % 3];
                    for _ in 0..ROUNDS {
                        if let Ok(permit) = l.acquire(None, weight).await {
                            let now = l.in_flight();
                            assert!(now <= biggest, "{now} units against a ceiling of {biggest}");
                            drop(permit);
                        }
                    }
                });
            }));
        }
        start.wait();
        for round in 0..ROUNDS {
            l.resize(SIZES[round % SIZES.len()]);
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let last = SIZES[(ROUNDS - 1) % SIZES.len()];
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.limit(), last);
        assert_eq!(l.available(), last, "the semaphore settled off its ceiling");
    }

    async fn hold(l: &Arc<Limiter>, n: usize, weight: u32) -> Vec<Permit> {
        let mut held = Vec::with_capacity(n);
        for _ in 0..n {
            held.push(l.acquire(None, weight).await.unwrap());
        }
        held
    }
}
