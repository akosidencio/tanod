//! Bounded retries for requests that are safe to send twice.
//!
//! A retry is extra origin load applied at exactly the moment the origin is
//! least able to absorb it. Every part of this module is therefore a bound,
//! and two of the bounds are not configurable because getting them wrong is
//! not a tuning mistake:
//!
//! * **Only safe methods.** `GET`, `HEAD`, `OPTIONS`, `TRACE`. `PUT` and
//!   `DELETE` are idempotent on paper, but Tanod does not buffer request
//!   bodies and so cannot replay one; and an origin that treats `POST` as
//!   idempotent is not a bet a proxy gets to make on its behalf.
//! * **Only before the origin has answered.** A connect failure, or an error
//!   on a reused keepalive connection with nothing yet written downstream.
//!   Once a byte of the response has left, the request is finished whatever
//!   happens next.
//!
//! The budget is the third bound and the one that matters during an incident.
//! Capping retries per *request* does nothing to protect an origin: a hundred
//! thousand requests each allowed one retry is still a doubling of load. The
//! budget caps them as a fraction of the traffic actually flowing, so a total
//! outage — where every request fails and would like to be retried — allows
//! almost none, while a single backend dying is absorbed.

use super::window::RollingRatio;
use crate::classifier::RequestClass;
use crate::config::schema::Retry as RetryConfig;
use http::Method;

/// Why a retry was or was not allowed. Every variant is a metric label, so
/// this set is deliberately small and bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retried. The request goes back through peer selection, which is what
    /// lets it land on a different backend.
    Allowed,
    /// The method, class, or configuration makes this request ineligible.
    Ineligible,
    /// Eligible, but it has already used `max_attempts`.
    AttemptsExhausted,
    /// Eligible, but the origin-wide budget is spent.
    BudgetExhausted,
}

impl RetryDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            RetryDecision::Allowed => "allowed",
            RetryDecision::Ineligible => "ineligible",
            RetryDecision::AttemptsExhausted => "attempts_exhausted",
            RetryDecision::BudgetExhausted => "budget_exhausted",
        }
    }

    pub fn allowed(self) -> bool {
        matches!(self, RetryDecision::Allowed)
    }
}

/// Is this request one Tanod may send to the origin a second time?
///
/// Both halves matter. The method must be safe, because that is the only
/// promise HTTP makes about sending a request twice. The class must not be one
/// whose whole point is a connection rather than a response: a `Streaming` or
/// `Upgrade` request that failed has usually failed *after* the client started
/// reading, and retrying it would produce two of something the client expects
/// one of.
pub fn eligible(method: &Method, class: RequestClass) -> bool {
    let safe = matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    let retryable_class = !matches!(
        class,
        RequestClass::Mutation | RequestClass::Streaming | RequestClass::Upgrade
    );
    safe && retryable_class
}

/// The origin-wide retry budget.
///
/// One per process, shared by every request, because the thing being protected
/// — the origin — is also shared. A per-route budget would let ten routes each
/// decide independently that a 10% retry rate was reasonable.
pub struct RetryBudget {
    enabled: bool,
    max_attempts: u32,
    percent: u64,
    minimum: u64,
    window: RollingRatio,
}

impl RetryBudget {
    pub fn new(cfg: &RetryConfig) -> Self {
        RetryBudget {
            enabled: cfg.enabled,
            max_attempts: cfg.max_attempts,
            percent: u64::from(cfg.budget_percent),
            minimum: u64::try_from(cfg.budget_min).unwrap_or(u64::MAX),
            window: RollingRatio::new(cfg.window.as_duration()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Count one origin request. This is the budget's denominator, so it is
    /// called for every attempt Tanod makes — including retries, which are
    /// themselves origin load.
    pub fn record_attempt(&self, now_ms: u64) {
        if self.enabled {
            self.window.record(now_ms, false);
        }
    }

    /// How many retries the current window allows.
    pub fn allowance(&self, now_ms: u64) -> u64 {
        let (total, _) = self.window.totals(now_ms);
        // `total * percent / 100`, floored, then lifted to the minimum so a
        // low-traffic deployment is not left with a budget that rounds to
        // zero and a retry setting that silently never fires.
        (total.saturating_mul(self.percent) / 100).max(self.minimum)
    }

    /// Spend one retry, if there is one to spend.
    ///
    /// `attempts_used` is how many times this request has already been sent,
    /// the first try included.
    pub fn charge(&self, now_ms: u64, attempts_used: u32) -> RetryDecision {
        if !self.enabled {
            return RetryDecision::Ineligible;
        }
        if attempts_used >= self.max_attempts {
            return RetryDecision::AttemptsExhausted;
        }
        // Take the slot first and give it back if it turns out not to fit.
        // Reading the budget and then incrementing would let a burst of
        // concurrent failures all observe the same under-budget reading and
        // all proceed, which is the exact stampede a budget exists to stop.
        let spent = self.window.flag(now_ms);
        if spent > self.allowance(now_ms) {
            self.window.unflag(now_ms);
            return RetryDecision::BudgetExhausted;
        }
        RetryDecision::Allowed
    }

    /// `(attempts, retries)` still inside the window, for the status document.
    pub fn counts(&self, now_ms: u64) -> (u64, u64) {
        self.window.totals(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::units::Dur;
    use std::time::Duration;

    fn budget(percent: u32, minimum: usize, max_attempts: u32) -> RetryBudget {
        RetryBudget::new(&RetryConfig {
            enabled: true,
            max_attempts,
            window: Dur(Duration::from_millis(1000)),
            budget_percent: percent,
            budget_min: minimum,
        })
    }

    #[test]
    fn only_safe_methods_are_ever_retried() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS, Method::TRACE] {
            assert!(
                eligible(&method, RequestClass::PublicDocument),
                "{method} should be retryable"
            );
        }
        for method in [
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::CONNECT,
        ] {
            assert!(
                !eligible(&method, RequestClass::PublicDocument),
                "{method} must never be retried"
            );
        }
    }

    #[test]
    fn connection_shaped_classes_are_never_retried() {
        for class in [
            RequestClass::Mutation,
            RequestClass::Streaming,
            RequestClass::Upgrade,
        ] {
            assert!(
                !eligible(&Method::GET, class),
                "{} was retryable",
                class.as_str()
            );
        }
    }

    #[test]
    fn retries_are_off_until_they_are_turned_on() {
        let b = RetryBudget::new(&RetryConfig::default());
        assert!(!b.enabled());
        assert_eq!(b.charge(0, 0), RetryDecision::Ineligible);
    }

    #[test]
    fn one_request_gets_at_most_max_attempts() {
        let b = budget(100, 100, 2);
        assert_eq!(b.charge(0, 1), RetryDecision::Allowed);
        assert_eq!(b.charge(0, 2), RetryDecision::AttemptsExhausted);
    }

    #[test]
    fn the_minimum_keeps_a_quiet_deployment_from_a_zero_budget() {
        // No traffic at all, so the percentage of it is zero.
        let b = budget(10, 3, 5);
        assert_eq!(b.allowance(0), 3);
        for _ in 0..3 {
            assert_eq!(b.charge(0, 1), RetryDecision::Allowed);
        }
        assert_eq!(b.charge(0, 1), RetryDecision::BudgetExhausted);
    }

    /// The property the budget exists for: retries cannot become a meaningful
    /// fraction of origin load, however many requests are failing.
    #[test]
    fn a_total_outage_cannot_double_the_load_on_the_origin() {
        let b = budget(10, 0, 5);
        for _ in 0..1000 {
            b.record_attempt(0);
        }
        assert_eq!(b.allowance(0), 100);

        let mut allowed = 0;
        for _ in 0..1000 {
            if b.charge(0, 1).allowed() {
                allowed += 1;
            }
        }
        assert_eq!(
            allowed, 100,
            "1000 failing requests were allowed {allowed} retries against a 10% budget"
        );
    }

    #[test]
    fn a_refused_retry_does_not_consume_budget() {
        let b = budget(10, 0, 5);
        for _ in 0..100 {
            b.record_attempt(0);
        }
        assert_eq!(b.allowance(0), 10);
        for _ in 0..50 {
            let _ = b.charge(0, 1);
        }
        assert_eq!(
            b.counts(0).1,
            10,
            "refused retries were left charged against the window"
        );
    }

    #[test]
    fn the_budget_refills_as_the_window_slides() {
        let b = budget(10, 0, 5);
        for _ in 0..100 {
            b.record_attempt(0);
        }
        for _ in 0..10 {
            assert!(b.charge(0, 1).allowed());
        }
        assert_eq!(b.charge(0, 1), RetryDecision::BudgetExhausted);

        // A full window later the old traffic and the old retries are both
        // gone, so the budget is back to its floor rather than to zero.
        let later = 2_000;
        for _ in 0..100 {
            b.record_attempt(later);
        }
        assert!(b.charge(later, 1).allowed());
    }
}
