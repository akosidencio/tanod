//! Observability. Never a hard dependency for serving a request: if metrics,
//! logging or span export fail, traffic continues.
//!
//! Four layers, deliberately independent of each other:
//!
//! * [`metrics`] — Prometheus. Aggregate, bounded cardinality, always on.
//! * [`logging`] — one structured line per request. Per-request, no sampling.
//! * [`trace`] — W3C trace context. Correlation, always on and free.
//! * [`otlp`] — span export. Sampled, optional, and the only one of the four
//!   that talks to another process.

pub mod json;
pub mod logging;
pub mod metrics;
pub mod otlp;
pub mod otlp_metrics;
pub mod trace;
pub mod transport;
