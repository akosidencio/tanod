//! Cache key construction and response shareability.
//!
//! Tanod implements Pingora's `Storage` trait rather than bringing its own
//! store, and uses `pingora-cache`'s cache lock for request coalescing. See
//! `spike/pingora-cache/FINDINGS.md` for how that was decided.

mod eviction;
pub mod key;
pub mod policy;
pub mod store;

pub use key::{CacheKey, KeyBuilder};
pub use policy::{BypassReason, Disposition, Shareability};
pub use store::{BoundedStore, Purged};

use pingora_cache::NoCacheReason;
use pingora_cache::predictor::{CacheablePredictor, Predictor};

/// Remembers the keys whose response outgrew `cache.max_body_size`, and
/// nothing else.
///
/// Pingora enforces that limit while the body streams. A response with a
/// `Content-Length` over it is never offered to the cache; a chunked one is
/// only discovered mid-body, after Pingora has started serving the client
/// from the cache, and it then aborts that response. Without a predictor the
/// next request for the URL does exactly the same, so every request for a
/// large streamed page fails. Remembering the key lets Pingora bypass the
/// cache for it from then on and stream the response uncached; it re-admits
/// the key itself if a later response fits again.
///
/// Every other reason is deliberately forgotten. Pingora's own predictor
/// would also remember `OriginNotCache` and custom reasons, which would turn
/// one private response into a bypass of the cache *and the coalescing lock*
/// for every later request to that URL.
pub struct OversizePredictor(Predictor<16>);

impl OversizePredictor {
    /// 16 shards of 1024 keys: about a quarter of a megabyte of `u128` keys
    /// and LRU links at most, evicted least-recently-marked first.
    pub fn new() -> Self {
        OversizePredictor(Predictor::new(1024, None))
    }
}

impl Default for OversizePredictor {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheablePredictor for OversizePredictor {
    fn cacheable_prediction(&self, key: &pingora_cache::CacheKey) -> bool {
        self.0.cacheable_prediction(key)
    }

    fn predicted_uncacheable_reason(&self, key: &pingora_cache::CacheKey) -> Option<NoCacheReason> {
        self.0.predicted_uncacheable_reason(key)
    }

    fn mark_cacheable(&self, key: &pingora_cache::CacheKey) -> bool {
        self.0.mark_cacheable(key)
    }

    fn mark_uncacheable(
        &self,
        key: &pingora_cache::CacheKey,
        reason: NoCacheReason,
    ) -> Option<bool> {
        match reason {
            NoCacheReason::ResponseTooLarge => self.0.mark_uncacheable(key, reason),
            _ => None,
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_oversized_response_is_remembered() {
        let predictor = OversizePredictor::new();
        let key = pingora_cache::CacheKey::new("/page", "/page");
        for reason in [
            NoCacheReason::OriginNotCache,
            NoCacheReason::Custom("memory_exhausted"),
            NoCacheReason::StorageError,
            NoCacheReason::InternalError,
        ] {
            assert_eq!(predictor.mark_uncacheable(&key, reason), None, "{reason:?}");
            assert!(
                predictor.cacheable_prediction(&key),
                "{reason:?} was remembered"
            );
        }
        assert_eq!(
            predictor.mark_uncacheable(&key, NoCacheReason::ResponseTooLarge),
            Some(true)
        );
        assert!(!predictor.cacheable_prediction(&key));
        assert_eq!(
            predictor.predicted_uncacheable_reason(&key),
            Some(NoCacheReason::ResponseTooLarge)
        );
        // A later response that fits makes it cacheable again. (Pingora's
        // return value here contradicts its own trait documentation, so only
        // the effect is asserted.)
        predictor.mark_cacheable(&key);
        assert!(predictor.cacheable_prediction(&key));
    }
}
