//! Cache key construction and response shareability.
//!
//! Tanod implements Pingora's `Storage` trait rather than bringing its own
//! store, and uses `pingora-cache`'s cache lock for request coalescing. See
//! `spike/pingora-cache/FINDINGS.md` for how that was decided.

pub mod key;
pub mod policy;
pub mod store;

pub use key::{CacheKey, KeyBuilder};
pub use policy::{BypassReason, Disposition, Shareability};
pub use store::{BoundedStore, Purged};

/// Private marker carried only in cache metadata while an in-flight response
/// is being followed. It is stripped before every downstream response.
pub(crate) const TRANSIENT_HEADER: &str = "x-tanod-transient-internal";

/// The longest request path Tanod remembers so it can be purged by path.
///
/// Generous for any real route and small enough that a client cannot grow the
/// cache's per-entry overhead by asking for very long URLs. Pingora's own
/// `CompactCacheKey` carries the same field and notes that its length "should
/// be small to keep memory usage bounded"; this is that bound, stated.
pub const MAX_PURGEABLE_PATH: usize = 512;
