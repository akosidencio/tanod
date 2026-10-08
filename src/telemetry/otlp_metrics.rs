//! Metric export over OTLP/HTTP, JSON encoding.
//!
//! Every `interval` the Prometheus registry is gathered and posted as one
//! `ExportMetricsServiceRequest`: the same series `/metrics` serves, pushed
//! instead of scraped. It exists so a deployment that sends telemetry to a
//! hosted backend needs no collector process beside Tanod whose only job is
//! to scrape it.
//!
//! The mapping is the one Prometheus-compatible backends reverse:
//!
//! * counter → monotonic cumulative `sum`, started at process start;
//! * gauge → `gauge`;
//! * histogram → cumulative `histogram`. Prometheus buckets are cumulative
//!   counts and OTLP buckets are per-bucket, so each bucket is the difference
//!   from the one below, and the last is everything above the top bound.
//!
//! Like span export, this is never load-bearing: a failed export is counted
//! and logged, never retried into a backlog, and never touches a request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use prometheus::proto::{MetricFamily, MetricType};

use super::json::{escape_into, quoted};
use super::metrics;
use super::otlp::unix_nanos;
use super::transport::{self, Transport};
use crate::config::schema::OtlpMetrics;

pub struct MetricsExporter {
    transport: Transport,
    interval: Duration,
    resource: Vec<(String, String)>,
    labels: Vec<(String, String)>,
    started_unix_nano: u64,
    /// Whether the last export succeeded, so a failure is logged at warn once
    /// when it starts rather than every minute while it lasts.
    healthy: AtomicBool,
}

/// Build the exporter. `resource` becomes the OTLP resource attributes and is
/// fixed at startup.
pub fn build(
    cfg: &OtlpMetrics,
    resource: Vec<(String, String)>,
) -> Result<MetricsExporter, String> {
    let endpoint = transport::parse_endpoint(&cfg.endpoint, "/v1/metrics")?;
    Ok(MetricsExporter {
        transport: Transport::new(endpoint, &cfg.headers, cfg.timeout.as_duration())?,
        interval: cfg.interval.as_duration(),
        resource,
        labels: cfg
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        started_unix_nano: unix_nanos(SystemTime::now()),
        healthy: AtomicBool::new(true),
    })
}

impl MetricsExporter {
    pub fn endpoint(&self) -> &transport::Endpoint {
        self.transport.endpoint()
    }

    async fn export(&self) {
        let body = encode(
            &self.resource,
            &self.labels,
            self.started_unix_nano,
            unix_nanos(SystemTime::now()),
            &prometheus::gather(),
        );
        match self.transport.post_json(&body).await {
            Ok(()) => {
                metrics::METRIC_EXPORTS
                    .with_label_values(&["exported"])
                    .inc();
                if !self.healthy.swap(true, Ordering::Relaxed) {
                    log::info!("metric export to {} recovered", self.endpoint().url());
                }
            }
            Err(why) => {
                metrics::METRIC_EXPORTS.with_label_values(&["failed"]).inc();
                if self.healthy.swap(false, Ordering::Relaxed) {
                    log::warn!("metric export to {} failed: {why}", self.endpoint().url());
                } else {
                    log::debug!("metric export still failing: {why}");
                }
            }
        }
    }
}

#[async_trait]
impl BackgroundService for MetricsExporter {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        log::info!(
            "exporting metrics to {} every {:?}",
            self.endpoint().url(),
            self.interval
        );
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires at once; the registry is all zeroes then.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    // One last export, bounded by the transport's own timeout,
                    // so the final minute before a deploy is not lost.
                    self.export().await;
                    return;
                }
                _ = ticker.tick() => self.export().await,
            }
        }
    }
}

/// Encode gathered families as an OTLP `ExportMetricsServiceRequest` in
/// protobuf-JSON: lowerCamelCase field names, 64-bit integers as strings.
pub fn encode(
    resource: &[(String, String)],
    labels: &[(String, String)],
    start_unix_nano: u64,
    now_unix_nano: u64,
    families: &[MetricFamily],
) -> String {
    let mut s = String::with_capacity(4096 + families.len() * 512);
    s.push_str("{\"resourceMetrics\":[{\"resource\":{\"attributes\":[");
    attributes(
        &mut s,
        resource.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    s.push_str("]},\"scopeMetrics\":[{\"scope\":{\"name\":\"tanod\",\"version\":\"");
    escape_into(&mut s, env!("CARGO_PKG_VERSION"));
    s.push_str("\"},\"metrics\":[");
    let mut first = true;
    for family in families {
        let kind = match family.type_() {
            MetricType::COUNTER => "sum",
            MetricType::GAUGE => "gauge",
            MetricType::HISTOGRAM => "histogram",
            // Tanod registers neither; anything else in the registry would
            // need a mapping of its own rather than a guess.
            _ => continue,
        };
        if family.metric.is_empty() {
            continue;
        }
        if !first {
            s.push(',');
        }
        first = false;
        s.push_str("{\"name\":");
        quoted(&mut s, family.name());
        s.push_str(",\"description\":");
        quoted(&mut s, family.help());
        s.push_str(",\"");
        s.push_str(kind);
        s.push_str("\":{");
        if kind != "gauge" {
            // 2 = AGGREGATION_TEMPORALITY_CUMULATIVE.
            s.push_str("\"aggregationTemporality\":2,");
        }
        if kind == "sum" {
            s.push_str("\"isMonotonic\":true,");
        }
        s.push_str("\"dataPoints\":[");
        let mut first_point = true;
        for metric in &family.metric {
            let value = match family.type_() {
                MetricType::COUNTER => metric.counter.value(),
                MetricType::GAUGE => metric.gauge.value(),
                _ => 0.0,
            };
            if !value.is_finite() {
                continue;
            }
            if !first_point {
                s.push(',');
            }
            first_point = false;
            s.push_str("{\"attributes\":[");
            attributes(
                &mut s,
                metric
                    .get_label()
                    .iter()
                    .map(|l| (l.name(), l.value()))
                    .chain(labels.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
            );
            s.push(']');
            if kind != "gauge" {
                s.push_str(&format!(",\"startTimeUnixNano\":\"{start_unix_nano}\""));
            }
            s.push_str(&format!(",\"timeUnixNano\":\"{now_unix_nano}\""));
            if kind == "histogram" {
                histogram_point(&mut s, &metric.histogram);
            } else {
                s.push_str(&format!(",\"asDouble\":{value}"));
            }
            s.push('}');
        }
        s.push_str("]}}");
    }
    s.push_str("]}]}]}");
    s
}

fn histogram_point(s: &mut String, h: &prometheus::proto::Histogram) {
    let buckets: Vec<_> = h
        .bucket
        .iter()
        .filter(|b| b.upper_bound().is_finite())
        .collect();
    let total = h.sample_count();
    s.push_str(&format!(",\"count\":\"{total}\""));
    let sum = h.sample_sum();
    if sum.is_finite() {
        s.push_str(&format!(",\"sum\":{sum}"));
    }
    s.push_str(",\"bucketCounts\":[");
    let mut below = 0u64;
    for bucket in &buckets {
        let cumulative = bucket.cumulative_count();
        s.push_str(&format!("\"{}\",", cumulative.saturating_sub(below)));
        below = cumulative;
    }
    // Everything above the top bound: the implicit +Inf bucket.
    s.push_str(&format!(
        "\"{}\"],\"explicitBounds\":[",
        total.saturating_sub(below)
    ));
    for (i, bucket) in buckets.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{}", bucket.upper_bound()));
    }
    s.push(']');
}

fn attributes<'a>(s: &mut String, pairs: impl Iterator<Item = (&'a str, &'a str)>) {
    for (i, (key, value)) in pairs.enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"key\":");
        quoted(s, key);
        s.push_str(",\"value\":{\"stringValue\":");
        quoted(s, value);
        s.push_str("}}");
    }
}

/// A per-process identity for `service.instance.id`: the hostname, which is
/// the pod or container name where Tanod usually runs. Without one, replicas
/// would report into the same series and overwrite each other.
pub fn instance_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| {
            let mut bytes = [0u8; 8];
            let _ = getrandom::fill(&mut bytes);
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{HistogramOpts, IntCounterVec, IntGauge, Opts, Registry};

    fn families() -> Vec<MetricFamily> {
        let registry = Registry::new();
        let counter =
            IntCounterVec::new(Opts::new("t_requests_total", "Requests"), &["route"]).unwrap();
        counter.with_label_values(&["home"]).inc_by(5);
        let gauge = IntGauge::new("t_in_flight", "In flight").unwrap();
        gauge.set(3);
        let histogram = prometheus::Histogram::with_opts(
            HistogramOpts::new("t_duration_seconds", "Duration").buckets(vec![0.1, 1.0]),
        )
        .unwrap();
        for v in [0.05, 0.5, 0.7, 5.0] {
            histogram.observe(v);
        }
        registry.register(Box::new(counter)).unwrap();
        registry.register(Box::new(gauge)).unwrap();
        registry.register(Box::new(histogram)).unwrap();
        registry.gather()
    }

    fn body() -> String {
        encode(
            &[("service.name".into(), "tanod".into())],
            &[("environment".into(), "staging".into())],
            1_000,
            2_000,
            &families(),
        )
    }

    #[test]
    fn the_document_is_valid_json_with_the_otlp_shape() {
        let body = body();
        let doc: serde_json::Value = serde_json::from_str(&body).expect(&body);
        let metrics = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        assert_eq!(metrics.as_array().unwrap().len(), 3);
        assert_eq!(
            doc["resourceMetrics"][0]["resource"]["attributes"][0]["key"],
            "service.name"
        );
    }

    #[test]
    fn a_counter_is_a_cumulative_monotonic_sum_with_its_labels_and_ours() {
        let doc: serde_json::Value = serde_json::from_str(&body()).unwrap();
        let m = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        let counter = m
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "t_requests_total")
            .unwrap();
        assert_eq!(counter["sum"]["aggregationTemporality"], 2);
        assert_eq!(counter["sum"]["isMonotonic"], true);
        let point = &counter["sum"]["dataPoints"][0];
        assert_eq!(point["asDouble"], 5.0);
        assert_eq!(point["startTimeUnixNano"], "1000");
        assert_eq!(point["timeUnixNano"], "2000");
        let attrs = point["attributes"].as_array().unwrap();
        assert_eq!(attrs[0]["key"], "route");
        assert_eq!(attrs[0]["value"]["stringValue"], "home");
        assert_eq!(attrs[1]["key"], "environment");
    }

    #[test]
    fn a_gauge_has_no_start_time_or_temporality() {
        let doc: serde_json::Value = serde_json::from_str(&body()).unwrap();
        let m = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        let gauge = m
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "t_in_flight")
            .unwrap();
        assert!(gauge["gauge"].get("aggregationTemporality").is_none());
        let point = &gauge["gauge"]["dataPoints"][0];
        assert!(point.get("startTimeUnixNano").is_none());
        assert_eq!(point["asDouble"], 3.0);
    }

    #[test]
    fn histogram_buckets_are_per_bucket_not_cumulative() {
        let doc: serde_json::Value = serde_json::from_str(&body()).unwrap();
        let m = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        let h = m
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "t_duration_seconds")
            .unwrap();
        let point = &h["histogram"]["dataPoints"][0];
        // 0.05 | 0.5, 0.7 | 5.0 against bounds 0.1 and 1.0.
        assert_eq!(point["bucketCounts"], serde_json::json!(["1", "2", "1"]));
        let bounds: Vec<f64> = point["explicitBounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_f64().unwrap())
            .collect();
        assert_eq!(bounds, [0.1, 1.0]);
        assert_eq!(point["count"], "4");
        assert!((point["sum"].as_f64().unwrap() - 6.25).abs() < 1e-9);
    }

    #[test]
    fn label_values_cannot_break_the_document() {
        let registry = Registry::new();
        let c = IntCounterVec::new(Opts::new("t_total", "x"), &["route"]).unwrap();
        c.with_label_values(&["a\"b\\c\n"]).inc();
        registry.register(Box::new(c)).unwrap();
        let body = encode(&[], &[], 0, 0, &registry.gather());
        let doc: serde_json::Value = serde_json::from_str(&body).expect(&body);
        let value = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
            [0]["attributes"][0]["value"]["stringValue"];
        assert_eq!(value, "a\"b\\c\n");
    }

    #[test]
    fn tanods_own_registry_encodes() {
        metrics::preregister();
        metrics::RESPONSES
            .with_label_values(&["r", "2xx", "origin"])
            .inc();
        let body = encode(&[], &[], 0, 1, &prometheus::gather());
        let _: serde_json::Value = serde_json::from_str(&body).expect(&body);
        assert!(body.contains("\"tanod_responses_total\""));
    }

    #[test]
    fn the_instance_id_is_never_empty() {
        assert!(!instance_id().is_empty());
    }
}
