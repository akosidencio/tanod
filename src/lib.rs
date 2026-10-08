//! Tanod — an origin workload governor for server-rendered applications.
//!
//! The name is Tagalog: a *tanod* is the barangay watchman, posted at the gate
//! to keep order so the community is never overrun. That is this crate's job
//! for an SSR origin.
//!
//! Three primitives carry the product, and all three are pure logic that can be
//! tested without a proxy runtime attached:
//!
//! * [`cache::key`] — cache key construction. Security-critical: a key that is
//!   too coarse serves one user's page to another.
//! * [`cache::policy`] — whether a response may be shared at all, evaluated
//!   both before and *after* the origin responds.
//! * [`admission`] — bounding how much origin work may be in flight.
//!
//! The Pingora proxy layer sits on top of these pure policy components.

pub mod admin;
pub mod admission;
pub mod cache;
pub mod classifier;
pub mod config;
pub mod net;
pub mod policy;
pub mod proxy;
pub mod supervise;
pub mod telemetry;
pub mod upstream;

pub use classifier::RequestClass;
pub use config::Config;

/// Entry points for the fuzz targets in `fuzz/`.
///
/// These are thin wrappers over internal parsers rather than a widening of the
/// public API: the functions worth fuzzing — query canonicalisation, `Vary`
/// evaluation, content-coding normalisation — are the ones a caller should
/// never reach directly, because reaching them means bypassing the policy that
/// decides whether they apply.
///
/// Enabled only by the `fuzzing` feature, which `fuzz/Cargo.toml` turns on.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use crate::classifier::RequestMetadata;
    use crate::config::schema::QueryPolicy;

    pub fn canonical_query(raw: &str, policy: Option<&QueryPolicy>) -> String {
        crate::cache::key::canonical_query(raw, policy)
    }

    pub fn normalize_accept_encoding(req: &RequestMetadata<'_>) -> String {
        crate::cache::key::normalize_accept_encoding(req)
    }

    pub fn unsupported_vary(vary: &str, key_headers: &[String]) -> Option<String> {
        crate::cache::policy::unsupported_vary(vary, key_headers)
    }
}
