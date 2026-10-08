//! Prometheus metrics.
//!
//! # Label discipline
//!
//! Route id is the only route-shaped label anywhere in this file. Never path,
//! never host, never query. A metrics label whose cardinality is controlled by
//! the client is a memory-exhaustion vector, and this is the one component
//! that must not fall over when traffic spikes — the same reason the cache has
//! a byte budget.
//!
//! Route ids come from the config file, so their cardinality is bounded by
//! something a person wrote.

// Every `expect` below is a `register_*!` inside a `LazyLock`. A failure means
// a duplicate metric name or a malformed label set — a mistake in this file,
// caught the first time the binary touches the metric, and with no runtime
// recovery worth writing. `expect` rather than `allow`: if these ever stop
// being the only panicking accessors here, the attribute itself goes stale
// and says so.
#![expect(
    clippy::expect_used,
    reason = "metric registration failure is a bug in this file, not a runtime condition"
)]

use prometheus::{
    HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, register_histogram_vec,
    register_int_counter, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
};
use std::sync::LazyLock;

/// Every label name Tanod's own metrics use. A test holds the registry to
/// this list, and pushed-metric labels from config may not reuse a name.
pub const LABEL_NAMES: &[&str] = &[
    "route",
    "class",
    "status",
    "reason",
    "decision",
    "upstream",
    "limiter",
    "outcome",
    "kind",
    "scope",
    "code",
    "source",
    "version",
    "deployment_id",
];

pub static REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_requests_total",
        "Requests received, by route and classification",
        &["route", "class"]
    )
    .expect("metric registration")
});

/// `status` is one of hit, miss, stale, bypass, shed, disabled — or
/// revalidate, for a background stale-while-revalidate fetch, which is origin
/// work but not a visitor request.
pub static CACHE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_cache_total",
        "Cache outcomes, by route and status",
        &["route", "status"]
    )
    .expect("metric registration")
});

/// Why a response was not shared. Bounded: the variants of `BypassReason`.
pub static BYPASS_REASON: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_cache_bypass_reason_total",
        "Reasons a response was not shared",
        &["route", "reason"]
    )
    .expect("metric registration")
});

/// `decision` is one of observe, admitted, exempt, shed_queue_full, shed_queue_timeout.
pub static ADMISSION: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_admission_total",
        "Admission decisions, by route",
        &["route", "decision"]
    )
    .expect("metric registration")
});

pub static ORIGIN_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_origin_requests_total",
        "Requests that reached the origin",
        &["route", "upstream"]
    )
    .expect("metric registration")
});

pub static ORIGIN_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "tanod_origin_latency_seconds",
        "Time the origin spent producing a response",
        &["route"],
        // Bucketed for server rendering, where 50ms is fast and 5s is a
        // problem — not for the sub-millisecond world the default buckets
        // assume.
        vec![
            0.005, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0
        ]
    )
    .expect("metric registration")
});

/// Responses as sent to visitors.
///
/// `code` is the status class (`2xx`..`5xx`, or `none` when the client left
/// before a response was written). `source` is who produced it: `origin`,
/// `cache`, or `tanod` — a shed, a refused upgrade, or a proxy error page.
/// Background revalidations are not visitor responses and are not counted.
pub static RESPONSES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_responses_total",
        "Responses sent to clients, by route, status class and source",
        &["route", "code", "source"]
    )
    .expect("metric registration")
});

/// Total time per request as the client sees it, from the first byte Tanod
/// read to the last it wrote. Cache hits are sub-millisecond, hence the low
/// buckets the origin histogram does not need.
pub static REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "tanod_request_duration_seconds",
        "Total time per request as the client sees it",
        &["route"],
        vec![
            0.001, 0.005, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0
        ]
    )
    .expect("metric registration")
});

/// Time spent waiting for an origin permit, for requests that went through
/// admission (admitted or shed). Exempt classes never wait and are not counted.
pub static QUEUE_WAIT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "tanod_queue_wait_seconds",
        "Time a request waited for admission",
        &["route"],
        vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0
        ]
    )
    .expect("metric registration")
});

/// Always 1; the labels carry what this process runs.
pub static BUILD_INFO: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_build_info",
        "Tanod version and configured deployment id; the value is always 1",
        &["version", "deployment_id"]
    )
    .expect("metric registration")
});

/// Publish the build-info series, replacing the previous one so a changed
/// `deployment.id` does not leave the old build exported beside the new.
pub fn set_build_info(deployment_id: Option<&str>) {
    BUILD_INFO.reset();
    BUILD_INFO
        .with_label_values(&[env!("CARGO_PKG_VERSION"), deployment_id.unwrap_or("")])
        .set(1);
}

/// `outcome` is exported or failed, one per OTLP metric export attempt.
pub static METRIC_EXPORTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_metric_exports_total",
        "OTLP metric export attempts, by outcome",
        &["outcome"]
    )
    .expect("metric registration")
});

/// The denominator of the origin-work-avoidance ratio.
///
/// Counted for a request only when reuse was actually possible for it: the
/// route permits reuse and the request was not bypass-classified. Without a
/// definition this precise the headline ratio can be improved by narrowing
/// eligibility, which makes it unfalsifiable and therefore worthless.
pub static REUSE_ELIGIBLE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_reuse_eligible_requests_total",
        "Requests for which reuse was possible; the denominator of the avoidance ratio",
        &["route"]
    )
    .expect("metric registration")
});

pub static QUEUE_DEPTH: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_queue_depth",
        "Requests currently waiting for origin capacity",
        &["limiter"]
    )
    .expect("metric registration")
});

pub static LIMIT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_concurrency_limit",
        "Configured origin concurrency ceiling",
        &["limiter"]
    )
    .expect("metric registration")
});

pub static IN_FLIGHT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_origin_in_flight",
        "Origin requests currently holding a permit",
        &["limiter"]
    )
    .expect("metric registration")
});

pub static SPOOL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_spool_total",
        "Responses that were spooled, by route and outcome",
        &["route", "reason"]
    )
    .expect("metric registration")
});

pub static SPOOL_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_spool_bytes",
        "Bytes currently held across every in-flight response spool"
    )
    .expect("metric registration")
});

pub static CACHE_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_cache_bytes",
        "Bytes held by the response cache, including fills in progress"
    )
    .expect("metric registration")
});

/// The configured cache budget.
///
/// Published so `tanod_cache_bytes` has a denominator. Occupancy on its own
/// is a number nobody can act on: "512MB used" is healthy or an emergency
/// depending entirely on the ceiling, and an alert that hardcodes the ceiling
/// goes stale the first time someone edits the config.
pub static CACHE_MAX_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_cache_max_bytes",
        "Configured cache.max_memory, the ceiling tanod_cache_bytes is measured against"
    )
    .expect("metric registration")
});

/// The configured spool budget, for the same reason.
pub static SPOOL_MAX_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_spool_max_bytes",
        "Configured spool.max_memory, the ceiling tanod_spool_bytes is measured against"
    )
    .expect("metric registration")
});

pub static CACHE_ENTRIES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_cache_entries",
        "Completed entries in the response cache"
    )
    .expect("metric registration")
});

pub static UPGRADES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_upgrade_total",
        "Protocol upgrade requests, by route and decision",
        &["route", "decision"]
    )
    .expect("metric registration")
});

pub static UPGRADES_ACTIVE: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_upgrade_active",
        "Upgraded connections currently held open"
    )
    .expect("metric registration")
});

/// Span lifecycle: `recorded`, `dropped` (a full queue), `exported`,
/// `export_failed`. Bounded: the four outcomes in [`super::otlp`].
///
/// `dropped` is the one to alert on. It is the signal that the tracing queue
/// is too small for the traffic, and it says so without the export path having
/// to slow a single request down to tell you.
pub static SPANS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_spans_total",
        "Spans by outcome: recorded, dropped, exported, export_failed",
        &["outcome"]
    )
    .expect("metric registration")
});

/// 1 while the process is draining, 0 otherwise.
///
/// A gauge rather than a counter because the question an operator asks during
/// a deploy is "is this instance still taking traffic", which is a state.
pub static DRAINING: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_draining",
        "1 while this instance is draining and reporting itself not ready"
    )
    .expect("metric registration")
});

/// The configuration generation currently in force. Increments on every
/// accepted `SIGHUP`; a reload that was refused leaves it unchanged, which is
/// what makes "did my config actually apply" answerable from a dashboard.
pub static CONFIG_GENERATION: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_config_generation",
        "Generation of the configuration currently in force"
    )
    .expect("metric registration")
});

/// Stable fingerprint of the effective configuration. Unlike generation, this
/// is comparable across replicas that have restarted or reloaded a different
/// number of times.
pub static CONFIG_FINGERPRINT: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_config_fingerprint",
        "Stable fingerprint of the effective configuration currently in force"
    )
    .expect("metric registration")
});

/// 1 per healthy upstream, 0 per unhealthy one. Labelled by the configured
/// address, which is config-derived like every other label in this file.
pub static UPSTREAM_HEALTHY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_upstream_healthy",
        "1 when a backend is passing its health check, 0 when it is not",
        &["upstream"]
    )
    .expect("metric registration")
});

/// How many addresses each upstream's name currently resolves to — the number
/// of origin instances Tanod is spreading work across. A drop to 1 when the
/// platform runs several instances means the others are not being used.
pub static UPSTREAM_ADDRESSES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_upstream_addresses",
        "Addresses an upstream name currently resolves to",
        &["upstream"]
    )
    .expect("metric registration")
});

/// 1 while a backend's circuit breaker is open, 0 while it is closed.
///
/// The companion to `tanod_upstream_healthy`, and the one that moves when an
/// origin is answering probes and failing renders. A backend that is healthy
/// and ejected at the same time is not a contradiction — it is the whole
/// reason passive observation exists, and seeing both series is how an
/// operator tells that story apart from a network partition.
pub static UPSTREAM_EJECTED: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_upstream_ejected",
        "1 when a backend's circuit breaker is open and it is out of rotation",
        &["upstream"]
    )
    .expect("metric registration")
});

/// Cumulative ejections per backend. A gauge says a backend is out now; this
/// says it has been out and back eleven times this hour, which is a different
/// and usually worse problem.
pub static UPSTREAM_TRIPS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_upstream_breaker_trips_total",
        "Times a backend's circuit breaker has opened",
        &["upstream"]
    )
    .expect("metric registration")
});

/// Origin failures as the breaker counts them. `kind` is one of `connect`,
/// `proxy`, `status` — bounded, and worth separating: a connect failure is a
/// dead process, a status failure is a live one that cannot do its job.
pub static UPSTREAM_FAILURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_upstream_failures_total",
        "Origin failures observed passively, by backend and kind",
        &["upstream", "kind"]
    )
    .expect("metric registration")
});

/// Origin requests currently outstanding to each backend. The input to
/// least-loaded selection, published so the routing decision is auditable
/// rather than a black box.
pub static UPSTREAM_IN_FLIGHT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_upstream_in_flight",
        "Origin requests currently outstanding to each backend",
        &["upstream"]
    )
    .expect("metric registration")
});

/// The other input to least-loaded selection: the exponentially weighted mean
/// time to first byte per backend.
///
/// Microseconds rather than seconds because the gauge is an integer, and a
/// 5ms origin would otherwise publish as zero.
pub static UPSTREAM_LATENCY_EWMA: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "tanod_upstream_latency_ewma_microseconds",
        "Exponentially weighted mean time to first byte per backend",
        &["upstream"]
    )
    .expect("metric registration")
});

/// Retry decisions. `outcome` is one of `allowed`, `ineligible`,
/// `attempts_exhausted`, `budget_exhausted` — the variants of
/// [`crate::upstream::retry::RetryDecision`].
///
/// `budget_exhausted` is the one to alert on: it means requests are failing
/// faster than the budget will absorb, which is an origin problem rather than
/// a retry-tuning problem.
pub static RETRIES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_origin_retries_total",
        "Retry decisions, by route and outcome",
        &["route", "outcome"]
    )
    .expect("metric registration")
});

/// Retries the budget would currently allow. The denominator for
/// `tanod_origin_retries_total{outcome="allowed"}`, published for the same
/// reason as `tanod_cache_max_bytes`: a count with no ceiling beside it is a
/// number nobody can act on.
pub static RETRY_BUDGET: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_origin_retry_budget",
        "Retries the current window's budget would allow"
    )
    .expect("metric registration")
});

/// Entries removed by an explicit purge, by `scope`: `tags` or `all`.
///
/// Deliberately separate from eviction. An evicted entry means the cache is
/// doing its job inside its budget; a purged one means somebody invalidated
/// something. A single counter covering both makes "why did our hit ratio
/// fall off a cliff" unanswerable.
pub static CACHE_PURGED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "tanod_cache_purged_total",
        "Cache entries removed by an explicit purge, by scope",
        &["scope"]
    )
    .expect("metric registration")
});

/// Entries discarded to stay inside `cache.max_memory`.
///
/// Read against `tanod_cache_bytes` and the hit ratio: eviction rising while
/// the cache is at its ceiling and the hit ratio is falling is the signal that
/// the working set does not fit.
pub static CACHE_EVICTED: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "tanod_cache_evicted_total",
        "Cache entries discarded to stay inside the byte budget"
    )
    .expect("metric registration")
});

/// Distinct invalidation tags currently indexed. The tag index is bounded by
/// the entries pointing at it, so this is the number that shows whether an
/// origin's tagging scheme is as small as its author thinks.
pub static CACHE_TAGS: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "tanod_cache_tags",
        "Distinct invalidation tags currently indexed"
    )
    .expect("metric registration")
});

/// Touch every metric family so a fresh scrape shows zeros rather than absent
/// series. A dashboard that renders "no data" during an incident is worse than
/// one that renders zero.
pub fn preregister() {
    LazyLock::force(&REQUESTS);
    LazyLock::force(&CACHE);
    LazyLock::force(&BYPASS_REASON);
    // A registered CounterVec with no label values is omitted from Prometheus
    // output. Seed one bounded zero-value series so a fresh deployment still
    // exposes the metric contract before its first cache bypass.
    BYPASS_REASON
        .with_label_values(&["-", "route_disabled"])
        .inc_by(0);
    LazyLock::force(&ADMISSION);
    LazyLock::force(&ORIGIN_REQUESTS);
    LazyLock::force(&ORIGIN_LATENCY);
    LazyLock::force(&REUSE_ELIGIBLE);
    LazyLock::force(&RESPONSES);
    LazyLock::force(&REQUEST_DURATION);
    LazyLock::force(&QUEUE_WAIT);
    LazyLock::force(&BUILD_INFO);
    LazyLock::force(&METRIC_EXPORTS);
    LazyLock::force(&QUEUE_DEPTH);
    LazyLock::force(&LIMIT);
    LazyLock::force(&IN_FLIGHT);
    LazyLock::force(&SPOOL);
    LazyLock::force(&SPOOL_BYTES);
    LazyLock::force(&CACHE_BYTES);
    LazyLock::force(&CACHE_MAX_BYTES);
    LazyLock::force(&SPOOL_MAX_BYTES);
    LazyLock::force(&CACHE_ENTRIES);
    LazyLock::force(&UPGRADES);
    LazyLock::force(&UPGRADES_ACTIVE);
    LazyLock::force(&SPANS);
    LazyLock::force(&DRAINING);
    LazyLock::force(&CONFIG_GENERATION);
    LazyLock::force(&CONFIG_FINGERPRINT);
    LazyLock::force(&UPSTREAM_HEALTHY);
    LazyLock::force(&UPSTREAM_ADDRESSES);
    LazyLock::force(&UPSTREAM_EJECTED);
    LazyLock::force(&UPSTREAM_TRIPS);
    LazyLock::force(&UPSTREAM_FAILURES);
    LazyLock::force(&UPSTREAM_IN_FLIGHT);
    LazyLock::force(&UPSTREAM_LATENCY_EWMA);
    LazyLock::force(&RETRIES);
    LazyLock::force(&RETRY_BUDGET);
    LazyLock::force(&CACHE_PURGED);
    LazyLock::force(&CACHE_EVICTED);
    LazyLock::force(&CACHE_TAGS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_registers_without_conflict() {
        // Duplicate registration panics at runtime; catch it here instead.
        preregister();
        preregister();
    }

    #[test]
    fn a_fresh_scrape_includes_the_cache_bypass_metric() {
        preregister();
        assert!(
            prometheus::gather()
                .iter()
                .any(|family| family.name() == "tanod_cache_bypass_reason_total")
        );
    }

    #[test]
    fn build_info_keeps_exactly_one_series() {
        set_build_info(Some("build-a"));
        set_build_info(Some("build-b"));
        let family = prometheus::gather()
            .into_iter()
            .find(|family| family.name() == "tanod_build_info")
            .unwrap();
        assert_eq!(family.metric.len(), 1);
        let labels = family.metric[0].get_label();
        assert!(
            labels
                .iter()
                .any(|l| l.name() == "deployment_id" && l.value() == "build-b")
        );
        assert!(
            labels
                .iter()
                .any(|l| l.name() == "version" && l.value() == env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn monotonic_evictions_are_exported_as_a_counter() {
        preregister();
        let family = prometheus::gather()
            .into_iter()
            .find(|family| family.name() == "tanod_cache_evicted_total")
            .unwrap();
        assert_eq!(family.type_(), prometheus::proto::MetricType::COUNTER);
    }

    #[test]
    fn no_metric_is_labelled_by_anything_client_controlled() {
        // Guards the rule in this module's docs. `upstream` and `limiter` are
        // config-derived, like `route`.
        let allowed = LABEL_NAMES;
        preregister();
        for family in prometheus::gather() {
            for metric in &family.metric {
                for label in &metric.label {
                    assert!(
                        allowed.contains(&label.name()),
                        "metric {} carries unexpected label `{}`",
                        family.name(),
                        label.name()
                    );
                }
            }
        }
    }
}
