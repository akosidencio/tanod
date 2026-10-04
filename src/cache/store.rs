//! A bounded in-memory implementation of Pingora's `Storage` trait.
//!
//! Pingora ships exactly one `Storage` implementation, `MemCache`, and it is
//! documented "for testing only, not for production use" for two concrete
//! reasons: it is an unbounded `HashMap`, and its `lookup_streaming_write` is
//! `.expect("must have partial write in progress")` — so a race between the
//! cache lock releasing and a late waiter's lookup takes the process down.
//! This implementation has a byte budget and returns a miss instead of
//! panicking.
//!
//! Waiters attach to a write *in progress* via the streaming-write tag, so a
//! coalesced request receives the leader's first chunk as the leader produces
//! it rather than waiting for the whole render. That is what keeps request
//! collapsing from destroying streaming.
//!
//! Both Pingora's per-response size limit and this store's global budget are
//! enforced while a fill is in progress. Abandoned fills remove their temporary
//! entry on drop.

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use pingora_cache::CacheMeta;
use pingora_cache::key::{CacheHashKey, CacheKey, CompactCacheKey};
use pingora_cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
    streaming_write::U64WriteId,
};
use pingora_cache::trace::SpanHandle;
use pingora_error::{Error, ErrorType, Result};
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::watch;

use crate::config::schema::{CacheDefaults, Eviction};

type BinaryMeta = (Vec<u8>, Vec<u8>);

/// Pingora wakes cache-lock readers and makes them perform another ordinary
/// lookup. A small handoff window keeps a just-completed transient response
/// visible long enough for those already-woken tasks to attach, without
/// turning it into a persistent cache entry.
const TRANSIENT_HANDOFF: std::time::Duration = std::time::Duration::from_millis(25);

/// Caps on what one response may add to the tag index.
///
/// The origin is trusted to say what a response *is*, not to be correct about
/// it. A buggy or compromised origin that answers with ten thousand tags per
/// response would otherwise grow the reverse index without bound, inside the
/// one component whose job is to keep working when things go wrong. Both
/// limits are generous for any real content model and neither is configurable,
/// because a deployment that needs more than this has a different problem.
const MAX_TAGS_PER_ENTRY: usize = 64;
const MAX_TAG_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    Partial(usize),
    Complete(usize),
}

impl Written {
    fn bytes(self) -> usize {
        match self {
            Written::Partial(n) | Written::Complete(n) => n,
        }
    }
    fn done(self) -> bool {
        matches!(self, Written::Complete(_))
    }
}

/// A finished entry.
struct Stored {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
    /// The request path this entry answers, so it can be purged by path.
    ///
    /// Empty when the path was too long to remember; see
    /// [`crate::cache::MAX_PURGEABLE_PATH`]. One entry per *variant* of a
    /// path — query strings, `Accept`, RSC — so purging a path removes all of
    /// them, which is what `revalidatePath()` means by a path.
    path: String,
    /// Invalidation tags this entry was stored under, as the origin declared
    /// them. Kept on the entry as well as in the reverse index so that
    /// evicting it can clean the index without scanning every tag.
    tags: Vec<String>,
    /// Read since this entry was last queued for eviction.
    ///
    /// An `AtomicBool` rather than a field behind the write lock: the hit path
    /// holds only a read lock, and taking a write lock on every cache hit to
    /// record recency is exactly the cost that makes true LRU unaffordable
    /// here. A relaxed store is enough — losing one bit under contention costs
    /// an entry one extra chance, not correctness.
    visited: AtomicBool,
}

impl Stored {
    fn size(&self) -> usize {
        self.meta.0.len()
            + self.meta.1.len()
            + self.body.read().len()
            + self.path.len()
            + self.tags.iter().map(String::len).sum::<usize>()
    }
}

/// A write in progress. Readers attach to this and follow it.
struct Temp {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
    transient: bool,
    tags: Vec<String>,
    path: String,
    /// A purge matched this fill while it was in progress. Existing followers
    /// may still drain the body, but it must not be admitted afterward.
    invalidated: bool,
}

impl Temp {
    fn new(meta: BinaryMeta, transient: bool, tags: Vec<String>, path: String) -> Self {
        let (tx, _rx) = watch::channel(Written::Partial(0));
        Temp {
            meta,
            body: Arc::new(RwLock::new(Vec::new())),
            written: Arc::new(tx),
            transient,
            tags,
            path,
            invalidated: false,
        }
    }
}

/// Split a tag header value into tags Tanod is willing to index.
///
/// Bounded on every axis: how many, how long, and what bytes. Non-ASCII and
/// whitespace-only tags are dropped rather than normalised — a tag is an
/// opaque identifier chosen by the origin, and silently rewriting one would
/// make a later purge for the original name miss.
fn parse_tags(value: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for raw in value.split(',') {
        let tag = raw.trim();
        if tag.is_empty() || tag.len() > MAX_TAG_LEN {
            continue;
        }
        if !tag.bytes().all(|b| b.is_ascii_graphic()) {
            continue;
        }
        if tags.iter().any(|existing| existing == tag) {
            continue;
        }
        tags.push(tag.to_string());
        if tags.len() >= MAX_TAGS_PER_ENTRY {
            break;
        }
    }
    tags
}

/// An in-memory store with a byte budget.
///
/// The budget is the whole point: `MemCache` is an unbounded map, which is the
/// substantive reason it is not for production, and a cache that cannot be
/// bounded is a memory-exhaustion vector in a component whose job is to absorb
/// traffic spikes.
pub struct BoundedStore {
    /// Makes registering and completing a fill atomic with respect to purge.
    /// The read side is held only for those short state transitions, never
    /// while response bytes stream.
    lifecycle: RwLock<()>,
    cached: RwLock<HashMap<String, Stored>>,
    /// Eviction queue. Front is the next candidate; under
    /// [`Eviction::Clock`] an entry that has been read gets requeued at the
    /// back instead of being discarded.
    order: RwLock<VecDeque<String>>,
    /// Reverse index from invalidation tag to the entries carrying it.
    ///
    /// A reverse index rather than a scan because purging is the operation an
    /// origin triggers, and an origin that publishes a product triggers it
    /// once per publish. A scan of every entry per purge would make
    /// invalidation cost scale with cache size, which is the wrong way round.
    tag_index: RwLock<HashMap<String, HashSet<String>>>,
    temp: RwLock<HashMap<String, HashMap<u64, Temp>>>,
    /// Includes completed entries and every byte in an in-progress fill.
    /// All memory-accounting mutations take this lock before cache-map locks.
    used: Mutex<usize>,
    max_bytes: usize,
    next_id: AtomicU64,
    policy: Eviction,
    /// Lowercased once at startup so the per-response lookup is a plain
    /// `HeaderMap::get`.
    tag_header: String,
    /// Entries discarded to make room, and entries discarded by an explicit
    /// purge. Separate counters: one is the cache working, the other is
    /// somebody invalidating, and confusing them makes a dashboard lie.
    evicted: AtomicU64,
    purged: AtomicU64,
}

/// Everything a purge removed, for the response and the metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Purged {
    /// Completed entries removed.
    pub entries: usize,
    /// Bytes returned to the budget.
    pub bytes: usize,
}

impl BoundedStore {
    pub fn new(cfg: &CacheDefaults) -> &'static Self {
        // `Storage` takes `&'static self` throughout (upstream marks this a
        // TODO). Leaking once at startup is the intended shape.
        Box::leak(Box::new(BoundedStore {
            lifecycle: RwLock::new(()),
            cached: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            tag_index: RwLock::new(HashMap::new()),
            temp: RwLock::new(HashMap::new()),
            used: Mutex::new(0),
            max_bytes: cfg.max_memory.as_usize(),
            next_id: AtomicU64::new(0),
            policy: cfg.eviction,
            tag_header: cfg.tag_header.to_ascii_lowercase(),
            evicted: AtomicU64::new(0),
            purged: AtomicU64::new(0),
        }))
    }

    /// The header this store reads invalidation tags from.
    pub fn tag_header(&self) -> &str {
        &self.tag_header
    }

    pub fn eviction(&self) -> Eviction {
        self.policy
    }

    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    pub fn purged(&self) -> u64 {
        self.purged.load(Ordering::Relaxed)
    }

    /// Distinct tags currently indexed.
    pub fn tags(&self) -> usize {
        self.tag_index.read().len()
    }

    pub fn bytes_used(&self) -> usize {
        *self.used.lock()
    }

    /// The configured byte budget. Published in the admin status document
    /// next to `bytes_used`, because occupancy without its ceiling is a number
    /// nobody can act on.
    pub fn limit(&self) -> usize {
        self.max_bytes
    }

    pub fn entries(&self) -> usize {
        self.cached.read().len()
    }

    /// Make room for `additional` bytes, evicting until the budget allows it.
    ///
    /// Lock order throughout this file is `used` → `cached` → `order` →
    /// `tag_index`, taken in that sequence and never the reverse. Every
    /// function that touches more than one of them takes them here or in
    /// `purge_matching` below, so the ordering is checkable by reading two
    /// places rather than ten.
    fn reserve(&self, additional: usize) -> bool {
        if additional > self.max_bytes {
            return false;
        }
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        let mut tag_index = self.tag_index.write();
        // Bounds the second-chance rotation. Without it, a working set that is
        // entirely hot could be requeued forever and this loop would never
        // return — a cache eviction policy is not allowed to be a livelock.
        // Once the budget is exhausted, later victims in this same call are
        // taken in FIFO order, which is the correct degradation: if everything
        // is hot, arrival order is as good a tiebreak as any.
        let mut chances = match self.policy {
            Eviction::Clock => order.len().saturating_add(1),
            Eviction::Fifo => 0,
        };
        while used.saturating_add(additional) > self.max_bytes {
            let Some(victim) = order.pop_front() else {
                return false;
            };
            let spared = chances > 0
                && cached
                    .get(&victim)
                    .is_some_and(|entry| entry.visited.swap(false, Ordering::Relaxed));
            if spared {
                chances -= 1;
                order.push_back(victim);
                continue;
            }
            if let Some(entry) = cached.remove(&victim) {
                *used = used.saturating_sub(entry.size());
                unindex_tags(&mut tag_index, &victim, &entry.tags);
                self.evicted.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::metrics::CACHE_EVICTED.inc();
            }
        }
        *used += additional;
        true
    }

    fn release(&self, amount: usize) {
        let mut used = self.used.lock();
        *used = used.saturating_sub(amount);
    }

    /// Convert an already-accounted temporary fill into a completed entry.
    fn admit(&self, key: String, obj: Stored) {
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        let mut tag_index = self.tag_index.write();
        // Remove before indexing the replacement. If both revisions carry the
        // same tag, doing this in the opposite order removes the new mapping
        // along with the old one.
        if let Some(old) = cached.remove(&key) {
            *used = used.saturating_sub(old.size());
            unindex_tags(&mut tag_index, &key, &old.tags);
            order.retain(|existing| existing != &key);
        }
        for tag in &obj.tags {
            tag_index
                .entry(tag.clone())
                .or_default()
                .insert(key.clone());
        }
        cached.insert(key.clone(), obj);
        order.push_back(key);
    }

    /// Drop every entry carrying any of these tags.
    ///
    /// Tags, not paths. A tag is what an origin already knows about its own
    /// content — "this page shows product 42" — whereas a path is what the
    /// URL space happens to look like today, and one product appears on
    /// several. Purging by tag is the operation that stays correct when the
    /// routing changes.
    pub fn purge_tags<'a>(&self, tags: impl IntoIterator<Item = &'a str>) -> Purged {
        let wanted: HashSet<&str> = tags.into_iter().filter(|tag| !tag.is_empty()).collect();
        if wanted.is_empty() {
            return Purged::default();
        }
        let _lifecycle = self.lifecycle.write();
        let mut victims: HashSet<String> = HashSet::new();
        {
            let tag_index = self.tag_index.read();
            for tag in &wanted {
                if let Some(keys) = tag_index.get(*tag) {
                    victims.extend(keys.iter().cloned());
                }
            }
        }
        {
            let mut temp = self.temp.write();
            for writes in temp.values_mut() {
                for fill in writes.values_mut() {
                    if fill.tags.iter().any(|tag| wanted.contains(tag.as_str())) {
                        fill.invalidated = true;
                    }
                }
            }
        }
        self.purge_matching_locked(|key, _| victims.contains(key))
    }

    /// Drop every completed entry.
    ///
    /// This is what a deployment rollover uses. The cache key already carries
    /// `deployment.id`, so entries from the previous build are unreachable the
    /// moment the id changes — but unreachable is not the same as reclaimed,
    /// and a cache still holding a whole build's worth of dead responses has
    /// that much less room for the one now serving traffic.
    pub fn purge_all(&self) -> Purged {
        let _lifecycle = self.lifecycle.write();
        {
            let mut temp = self.temp.write();
            for writes in temp.values_mut() {
                for fill in writes.values_mut() {
                    fill.invalidated = true;
                }
            }
        }
        self.purge_matching_locked(|_, _| true)
    }

    /// Drop every entry answering any of these exact paths.
    ///
    /// This is `revalidatePath()`'s shape: one path, every variant of it. A
    /// single route is several entries — query strings, `Accept`, the RSC
    /// flight payload beside the HTML — and invalidating the page means
    /// invalidating all of them, so matching is on the path alone.
    ///
    /// A scan rather than a second reverse index. Tags are indexed because one
    /// tag legitimately spans hundreds of entries; paths are compared directly
    /// because the path is already on the entry for no extra memory, and the
    /// alternative is a second index to maintain on every admit and every
    /// eviction. Entry count is bounded by the byte budget, so the scan is
    /// bounded too. If a profile ever says otherwise, an index is a drop-in.
    ///
    /// Matching is exact and **not** percent-decoded, for the same reason as
    /// tags: the stored path is the request path as it arrived, and decoding
    /// here would let a purge miss the entry it was aimed at.
    pub fn purge_paths<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Purged {
        let wanted: HashSet<&str> = paths.into_iter().filter(|p| !p.is_empty()).collect();
        if wanted.is_empty() {
            return Purged::default();
        }
        let _lifecycle = self.lifecycle.write();
        {
            let mut temp = self.temp.write();
            for writes in temp.values_mut() {
                for fill in writes.values_mut() {
                    if wanted.contains(fill.path.as_str()) {
                        fill.invalidated = true;
                    }
                }
            }
        }
        self.purge_matching_locked(|_, entry| wanted.contains(entry.path.as_str()))
    }

    /// The one place entries leave the cache for a reason other than eviction.
    ///
    /// The caller holds the write side of `lifecycle`, so no persistent fill
    /// can move between `temp` and `cached` while the match is evaluated.
    /// Matching temporary fills are marked separately: their clients finish
    /// normally, but the response is not admitted afterward.
    fn purge_matching_locked(&self, matches: impl Fn(&str, &Stored) -> bool) -> Purged {
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        let mut tag_index = self.tag_index.write();

        let doomed: Vec<String> = cached
            .iter()
            .filter(|(key, entry)| matches(key, entry))
            .map(|(key, _)| key.clone())
            .collect();
        let mut purged = Purged::default();
        for key in &doomed {
            if let Some(entry) = cached.remove(key) {
                let size = entry.size();
                *used = used.saturating_sub(size);
                purged.entries += 1;
                purged.bytes += size;
                unindex_tags(&mut tag_index, key, &entry.tags);
            }
        }
        if !doomed.is_empty() {
            let removed: HashSet<&String> = doomed.iter().collect();
            order.retain(|key| !removed.contains(key));
            self.purged
                .fetch_add(purged.entries as u64, Ordering::Relaxed);
        }
        purged
    }

    fn remove_temp(&self, key: &str, id: u64) -> Option<Temp> {
        let mut temp = self.temp.write();
        let removed = temp.get_mut(key).and_then(|writes| writes.remove(&id));
        if temp.get(key).is_some_and(HashMap::is_empty) {
            temp.remove(key);
        }
        removed
    }
}

/// Remove one key from every tag it was indexed under, dropping tags that no
/// longer point at anything.
///
/// Dropping the empty set matters: without it the index keeps one entry per
/// tag name ever seen, which for a content model that mints a tag per product
/// revision is an unbounded leak of exactly the kind the byte budget exists to
/// prevent.
fn unindex_tags(index: &mut HashMap<String, HashSet<String>>, key: &str, tags: &[String]) {
    for tag in tags {
        if let Some(keys) = index.get_mut(tag) {
            keys.remove(key);
            if keys.is_empty() {
                index.remove(tag);
            }
        }
    }
}

#[async_trait]
impl Storage for BoundedStore {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();

        // Recording the read here rather than in the hit handler is
        // deliberate: this is the point at which the entry demonstrably had
        // value, whether or not the client goes on to read the whole body.
        let found = {
            let cached = self.cached.read();
            cached.get(&hash).map(|obj| {
                obj.visited.store(true, Ordering::Relaxed);
                (obj.meta.clone(), obj.body.clone())
            })
        };
        if let Some((meta, body)) = found {
            let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
            return Ok(Some((meta, Box::new(CompleteHit { body, read: 0 }))));
        }

        // Nothing finished — but a write may be in progress, and this is the
        // path a coalesced waiter arrives on.
        //
        // When a storage supports streaming partial writes, Pingora releases
        // the cache lock as soon as the leader's miss handler exists, so
        // waiters wake with `LockStatus::Done` and come straight back here
        // with no write tag. If `lookup` only ever answered from finished
        // entries, every one of them would miss and go to the origin — which
        // is request collapsing failing silently, in the one shape where the
        // collapsing matters most.
        let partial = {
            let temp = self.temp.read();
            temp.get(&hash).and_then(|writes| {
                // One leader per key in practice; take the newest if a
                // revalidation overlaps.
                writes
                    .iter()
                    .max_by_key(|(id, _)| **id)
                    .map(|(_, t)| (t.meta.clone(), t.body.clone(), t.written.subscribe()))
            })
        };
        let Some((meta, body, written)) = partial else {
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(PartialHit {
                body,
                written,
                read: 0,
            }),
        )))
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        t: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let Some(tag) = streaming_write_tag else {
            return self.lookup(key, t).await;
        };
        let Ok(write_id): std::result::Result<U64WriteId, _> = tag.try_into() else {
            // MemCache panics here. A malformed tag is not a reason to take
            // the process down.
            return Ok(None);
        };
        let id: u64 = write_id.into();
        let hash = key.combined();

        // Take everything needed out of the lock in one scope: a parking_lot
        // guard is !Send, so holding one across the await below makes the
        // whole future non-Send.
        let found = {
            let guard = self.temp.read();
            guard.get(&hash).and_then(|m| m.get(&id)).map(|temp| {
                (
                    temp.meta.clone(),
                    temp.body.clone(),
                    temp.written.subscribe(),
                )
            })
        };
        let Some((meta, body, written)) = found else {
            // Pingora requires an exact tag match here. Falling back to another
            // completed or in-progress write could attach the leader itself to
            // a different response for the same key.
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(PartialHit {
                body,
                written,
                read: 0,
            }),
        )))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<MissHandler> {
        // A purge either sees this fill in `temp` or completes before the fill
        // is registered; there is no gap in which both can miss each other.
        let _lifecycle = self.lifecycle.read();
        let hash = key.combined();
        let transient = meta
            .response_header()
            .headers
            .contains_key(crate::cache::TRANSIENT_HEADER);
        // Read the origin's tags here, where the response header is still in
        // hand. A transient entry is never admitted, so its tags would only be
        // accounted and thrown away.
        let tags = if transient {
            Vec::new()
        } else {
            meta.response_header()
                .headers
                .get(&self.tag_header)
                .and_then(|value| value.to_str().ok())
                .map(parse_tags)
                .unwrap_or_default()
        };
        // The path travels in the key's `user_tag`, which Pingora does not
        // hash. See `Tanod::cache_key_callback`.
        let path = if transient {
            String::new()
        } else {
            key.user_tag().to_string()
        };
        let serialized = meta.serialize()?;
        let tags_size: usize = tags.iter().map(String::len).sum();
        let meta_size = serialized.0.len() + serialized.1.len() + tags_size + path.len();
        if !self.reserve(meta_size) {
            return Error::e_explain(ErrorType::InternalError, "cache memory budget exhausted");
        }
        let temp = Temp::new(serialized, transient, tags, path);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handler = FollowableMiss {
            store: self,
            key: hash.clone(),
            id: id.into(),
            body: temp.body.clone(),
            written: temp.written.clone(),
            accounted: meta_size,
            finished: false,
        };
        self.temp.write().entry(hash).or_default().insert(id, temp);
        Ok(Box::new(handler))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        _p: PurgeType,
        _t: &SpanHandle,
    ) -> Result<bool> {
        // Pingora's own purge hook, reached through its cache API rather than
        // through Tanod's endpoint. Routed into the same path so a caller
        // that arrives this way cannot leave the tag index pointing at an
        // entry that no longer exists.
        let hash = key.combined();
        let _lifecycle = self.lifecycle.write();
        let temp_invalidated = self.temp.write().get_mut(&hash).is_some_and(|writes| {
            for fill in writes.values_mut() {
                fill.invalidated = true;
            }
            !writes.is_empty()
        });
        let completed = self.purge_matching_locked(|key, _| key == hash).entries > 0;
        Ok(temp_invalidated || completed)
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<bool> {
        let _lifecycle = self.lifecycle.read();
        let hash = key.combined();
        let replacement = meta.serialize()?;
        let replacement_tags = meta
            .response_header()
            .headers
            .get(&self.tag_header)
            .and_then(|value| value.to_str().ok())
            .map(parse_tags)
            .unwrap_or_default();
        let replacement_size = replacement.0.len()
            + replacement.1.len()
            + replacement_tags.iter().map(String::len).sum::<usize>();
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut tag_index = self.tag_index.write();
        let Some(obj) = cached.get_mut(&hash) else {
            return Ok(false);
        };
        let old_size =
            obj.meta.0.len() + obj.meta.1.len() + obj.tags.iter().map(String::len).sum::<usize>();
        if replacement_size > old_size {
            let growth = replacement_size - old_size;
            if used.saturating_add(growth) > self.max_bytes {
                return Ok(false);
            }
            *used += growth;
        } else {
            *used = used.saturating_sub(old_size - replacement_size);
        }
        unindex_tags(&mut tag_index, &hash, &obj.tags);
        for tag in &replacement_tags {
            tag_index
                .entry(tag.clone())
                .or_default()
                .insert(hash.clone());
        }
        obj.meta = replacement;
        obj.tags = replacement_tags;
        Ok(true)
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}

/// Reads a finished entry.
struct CompleteHit {
    body: Arc<RwLock<Vec<u8>>>,
    read: usize,
}

#[async_trait]
impl HandleHit for CompleteHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        const CHUNK: usize = 64 * 1024;
        let body = self.body.read();
        if self.read >= body.len() {
            return Ok(None);
        }
        let end = (self.read + CHUNK).min(body.len());
        let chunk = Bytes::copy_from_slice(&body[self.read..end]);
        self.read = end;
        Ok(Some(chunk))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        true
    }

    fn seek(&mut self, start: usize, _end: Option<usize>) -> Result<()> {
        self.read = start.min(self.body.read().len());
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// Reads a write that is still in progress, following it chunk by chunk.
///
/// This is what decides whether coalescing destroys streaming. A waiter here
/// receives the leader's first chunk as soon as the leader produces it, rather
/// than waiting for the whole render to finish.
struct PartialHit {
    body: Arc<RwLock<Vec<u8>>>,
    written: watch::Receiver<Written>,
    read: usize,
}

#[async_trait]
impl HandleHit for PartialHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        loop {
            let state = *self.written.borrow_and_update();
            let available = state.bytes();
            if available > self.read {
                // Copy out under the lock, then release it before awaiting.
                let end = (self.read + 64 * 1024).min(available);
                let chunk = Bytes::copy_from_slice(&self.body.read()[self.read..end]);
                self.read = end;
                return Ok(Some(chunk));
            }
            if state.done() {
                return Ok(None);
            }
            if self.written.changed().await.is_err() {
                // The writer vanished without finishing.
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "cache writer dropped",
                ));
            }
        }
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        false
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// The leader's writer. Publishes progress so waiters can follow.
struct FollowableMiss {
    store: &'static BoundedStore,
    key: String,
    id: U64WriteId,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
    accounted: usize,
    finished: bool,
}

#[async_trait]
impl HandleMiss for FollowableMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        if !data.is_empty() && !self.store.reserve(data.len()) {
            return Error::e_explain(
                ErrorType::InternalError,
                "cache memory budget exhausted during fill",
            );
        }
        self.accounted += data.len();
        let so_far = self.written.borrow().bytes();
        self.body.write().extend_from_slice(&data);
        let total = so_far + data.len();
        self.written.send_replace(if eof {
            Written::Complete(total)
        } else {
            Written::Partial(total)
        });
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<MissFinishType> {
        let id: u64 = self.id.into();
        // Make sure any follower that has not drained yet sees the end.
        let total = self.written.borrow().bytes();
        self.written.send_replace(Written::Complete(total));

        let transient = {
            let temp = self.store.temp.read();
            temp.get(&self.key)
                .and_then(|writes| writes.get(&id))
                .map(|entry| entry.transient)
        };
        let Some(transient) = transient else {
            return Err(Error::explain(ErrorType::InternalError, "write vanished"));
        };
        let size = self.body.read().len();
        self.finished = true;
        if transient {
            // The lock is released when the miss handler is created. If the
            // whole body arrives in the same upstream read as the headers,
            // removing this temp immediately races the woken followers and
            // turns each into a new origin request. Keep it only for a short
            // scheduler handoff; attached readers retain their own Arcs.
            let store = self.store;
            let key = self.key.clone();
            let accounted = self.accounted;
            tokio::spawn(async move {
                tokio::time::sleep(TRANSIENT_HANDOFF).await;
                if store.remove_temp(&key, id).is_some() {
                    store.release(accounted);
                }
            });
        } else {
            // A purge either marks this fill before this transition or waits
            // until it has become a completed entry and removes it there.
            let _lifecycle = self.store.lifecycle.read();
            let temp = self
                .store
                .remove_temp(&self.key, id)
                .ok_or_else(|| Error::explain(ErrorType::InternalError, "write vanished"))?;
            if temp.invalidated {
                self.store.release(self.accounted);
                return Ok(MissFinishType::Created(size));
            }
            let body = temp.body;
            self.store.admit(
                self.key.clone(),
                Stored {
                    meta: temp.meta,
                    body,
                    tags: temp.tags,
                    path: temp.path,
                    // A freshly stored entry starts unvisited. The request
                    // that filled it does not count as a read: one arrival is
                    // exactly the evidence FIFO already has, and crediting it
                    // here would give every entry a free pass and turn the
                    // policy back into FIFO with extra steps.
                    visited: AtomicBool::new(false),
                },
            );
        }
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.id.as_bytes())
    }
}

impl Drop for FollowableMiss {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let id: u64 = self.id.into();
        if self.store.remove_temp(&self.key, id).is_some() {
            self.store.release(self.accounted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Aliased: `Bytes` in this file already means `bytes::Bytes`, and the
    // body-writing tests below use it.
    use crate::config::units::Bytes as ByteSize;

    fn cache_cfg(max_bytes: usize) -> CacheDefaults {
        CacheDefaults {
            max_memory: ByteSize(max_bytes as u64),
            ..Default::default()
        }
    }

    /// A store with a byte budget and otherwise default policy.
    fn store_of(max_bytes: usize) -> &'static BoundedStore {
        BoundedStore::new(&cache_cfg(max_bytes))
    }
    use pingora_cache::CacheMeta;
    use pingora_cache::trace::Span;
    use pingora_http::ResponseHeader;
    use std::time::{Duration, SystemTime};

    fn meta() -> CacheMeta {
        let now = SystemTime::now();
        CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            0,
            0,
            ResponseHeader::build(200, None).unwrap(),
        )
    }

    /// Store one entry of `body_len` bytes, tagged as given.
    async fn fill(store: &'static BoundedStore, path: &str, tags: &[&str], body_len: usize) {
        let mut header = ResponseHeader::build(200, None).unwrap();
        if !tags.is_empty() {
            header
                .insert_header("x-tanod-cache-tags", tags.join(","))
                .unwrap();
        }
        let now = SystemTime::now();
        let meta = CacheMeta::new(now + Duration::from_secs(600), now, 0, 0, header);
        // `user_tag` carries the request path, exactly as
        // `Tanod::cache_key_callback` sets it.
        let key = CacheKey::new("", path, path);
        let mut writer = store
            .get_miss_handler(&key, &meta, &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from(vec![b'x'; body_len]), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
    }

    /// Store an entry whose key and path differ, so a purge by path is tested
    /// against the path rather than against the key it happens to share.
    async fn fill_variant(store: &'static BoundedStore, key: &str, path: &str) {
        let now = SystemTime::now();
        let meta = CacheMeta::new(
            now + Duration::from_secs(600),
            now,
            0,
            0,
            ResponseHeader::build(200, None).unwrap(),
        );
        let mut writer = store
            .get_miss_handler(
                &CacheKey::new("", key, path),
                &meta,
                &Span::inactive().handle(),
            )
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"body"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
    }

    async fn is_cached(store: &'static BoundedStore, path: &str) -> bool {
        is_key_cached(store, path).await
    }

    async fn is_key_cached(store: &'static BoundedStore, key: &str) -> bool {
        store
            .lookup(&CacheKey::new("", key, ""), &Span::inactive().handle())
            .await
            .unwrap()
            .is_some()
    }

    // ---------------------------------------------------------- purge by path

    /// The property that makes carrying the path safe at all.
    ///
    /// Cache keys decide which client may see which response, so the one thing
    /// this feature must not do is change them. Pingora hashes only the
    /// namespace and the primary key, so `user_tag` is free — this pins that,
    /// because if a future Pingora ever folded `user_tag` into the hash, every
    /// stored entry would silently move and this would be the test that said so.
    #[test]
    fn carrying_the_path_does_not_change_the_cache_key() {
        use pingora_cache::key::CacheHashKey;
        let bare = CacheKey::new("", "/canonical-string", "");
        let tagged = CacheKey::new("", "/canonical-string", "/products/iphone");
        assert_eq!(bare.combined(), tagged.combined());
        assert_eq!(bare.primary(), tagged.primary());
    }

    #[tokio::test]
    async fn purging_a_path_removes_every_variant_of_it() {
        let store = store_of(1 << 20);
        // One route, three entries: the document, a query variant, and the
        // RSC payload. All three answer the same path.
        fill_variant(store, "html:/products/iphone", "/products/iphone").await;
        fill_variant(store, "query:/products/iphone?ref=x", "/products/iphone").await;
        fill_variant(store, "rsc:/products/iphone", "/products/iphone").await;
        fill_variant(store, "html:/products/pixel", "/products/pixel").await;
        assert_eq!(store.entries(), 4);

        let purged = store.purge_paths(["/products/iphone"]);
        assert_eq!(
            purged.entries, 3,
            "invalidating a page has to invalidate every variant of it"
        );
        assert!(is_key_cached(store, "html:/products/pixel").await);
        assert!(!is_key_cached(store, "rsc:/products/iphone").await);
    }

    #[tokio::test]
    async fn purging_a_path_is_exact_not_a_prefix() {
        let store = store_of(1 << 20);
        fill_variant(store, "a", "/products").await;
        fill_variant(store, "b", "/products/iphone").await;
        assert_eq!(store.purge_paths(["/products"]).entries, 1);
        assert!(
            is_key_cached(store, "b").await,
            "an exact path purge swept a subtree"
        );
    }

    #[tokio::test]
    async fn purging_an_unknown_path_changes_nothing() {
        let store = store_of(1 << 20);
        fill_variant(store, "a", "/known").await;
        assert_eq!(store.purge_paths(["/unknown"]), Purged::default());
        assert!(is_key_cached(store, "a").await);
    }

    #[tokio::test]
    async fn purging_several_paths_at_once() {
        let store = store_of(1 << 20);
        for n in 0..4 {
            fill_variant(store, &format!("k{n}"), &format!("/p/{n}")).await;
        }
        assert_eq!(store.purge_paths(["/p/0", "/p/2"]).entries, 2);
        assert_eq!(store.entries(), 2);
    }

    #[tokio::test]
    async fn a_path_purge_prevents_a_matching_in_progress_fill_from_being_admitted() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "path-fill", "/purged-path");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();

        assert_eq!(store.purge_paths(["/purged-path"]), Purged::default());
        writer.write_body(Bytes::new(), true).await.unwrap();
        writer.finish().await.unwrap();

        assert!(!is_key_cached(store, "path-fill").await);
        assert_eq!(store.bytes_used(), 0);
    }

    /// An entry whose path was too long to remember has an empty path, and an
    /// empty purge target must never match it — otherwise one over-long URL
    /// makes every such entry collateral damage.
    #[tokio::test]
    async fn an_entry_with_no_remembered_path_is_not_purged_by_an_empty_path() {
        let store = store_of(1 << 20);
        fill_variant(store, "unpurgeable", "").await;
        assert_eq!(store.purge_paths([""]), Purged::default());
        assert_eq!(store.purge_paths(["", "/nothing"]), Purged::default());
        assert!(is_key_cached(store, "unpurgeable").await);
        // It is still reachable by the blunt instrument.
        assert_eq!(store.purge_all().entries, 1);
    }

    #[tokio::test]
    async fn purging_a_path_returns_its_bytes_to_the_budget() {
        let store = store_of(1 << 20);
        fill(store, "/page", &[], 4096).await;
        assert!(store.bytes_used() >= 4096);
        assert_eq!(store.purge_paths(["/page"]).entries, 1);
        assert_eq!(store.bytes_used(), 0);
    }

    #[tokio::test]
    async fn a_path_purge_also_cleans_the_tag_index() {
        let store = store_of(1 << 20);
        fill(store, "/page", &["a-tag"], 64).await;
        assert_eq!(store.tags(), 1);
        store.purge_paths(["/page"]);
        assert_eq!(store.tags(), 0, "a path purge left the tag behind");
    }

    // ----------------------------------------------------------- cache tags

    #[tokio::test]
    async fn an_entry_is_indexed_under_every_tag_the_origin_declared() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["product-1", "category-shoes"], 32).await;
        assert_eq!(store.tags(), 2);
        assert_eq!(store.entries(), 1);
    }

    #[tokio::test]
    async fn purging_a_tag_removes_every_entry_carrying_it() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["product-1", "category-shoes"], 32).await;
        fill(store, "/p/2", &["product-2", "category-shoes"], 32).await;
        fill(store, "/about", &["static-page"], 32).await;

        let purged = store.purge_tags(["category-shoes"]);
        assert_eq!(purged.entries, 2);
        assert!(purged.bytes > 0);
        assert!(!is_cached(store, "/p/1").await);
        assert!(!is_cached(store, "/p/2").await);
        assert!(
            is_cached(store, "/about").await,
            "an entry without that tag was collateral damage"
        );
    }

    #[tokio::test]
    async fn purging_returns_the_bytes_to_the_budget() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["t"], 4096).await;
        assert!(store.bytes_used() >= 4096);
        store.purge_tags(["t"]);
        assert_eq!(store.bytes_used(), 0, "purged bytes were never released");
        assert_eq!(store.entries(), 0);
    }

    /// A tag index that keeps every tag name it has ever seen is an unbounded
    /// leak, in a store whose whole point is being bounded.
    #[tokio::test]
    async fn a_tag_pointing_at_nothing_is_dropped_from_the_index() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["revision-1"], 32).await;
        assert_eq!(store.tags(), 1);
        store.purge_tags(["revision-1"]);
        assert_eq!(store.tags(), 0, "the empty tag set stayed in the index");
    }

    #[tokio::test]
    async fn eviction_also_cleans_the_tag_index() {
        let store = store_of(budget_for(2, 1500).await);
        fill(store, "/p/1", &["gone"], 1500).await;
        fill(store, "/p/2", &["kept"], 1500).await;
        fill(store, "/p/3", &["kept-too"], 1500).await;
        assert!(!is_cached(store, "/p/1").await, "nothing was evicted");
        assert!(
            !store.tag_index.read().contains_key("gone"),
            "an evicted entry left its tag behind"
        );
    }

    #[tokio::test]
    async fn purging_everything_empties_the_cache_and_the_index() {
        let store = store_of(1 << 20);
        for n in 0..5 {
            fill(store, &format!("/p/{n}"), &[&format!("t{n}")], 64).await;
        }
        let purged = store.purge_all();
        assert_eq!(purged.entries, 5);
        assert_eq!(store.entries(), 0);
        assert_eq!(store.tags(), 0);
        assert_eq!(store.bytes_used(), 0);
    }

    #[tokio::test]
    async fn purging_an_unknown_tag_changes_nothing() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["real"], 64).await;
        assert_eq!(store.purge_tags(["imaginary"]), Purged::default());
        assert!(is_cached(store, "/p/1").await);
    }

    #[tokio::test]
    async fn a_re_stored_entry_does_not_leave_a_stale_tag_pointing_at_it() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["v1"], 64).await;
        fill(store, "/p/1", &["v2"], 64).await;
        assert_eq!(store.entries(), 1);
        // The first revision's tag must not still name this entry, or a purge
        // of a tag it no longer carries would drop it.
        assert_eq!(store.purge_tags(["v1"]), Purged::default());
        assert!(is_cached(store, "/p/1").await);
        assert_eq!(store.purge_tags(["v2"]).entries, 1);
    }

    #[tokio::test]
    async fn a_re_stored_entry_keeps_tags_shared_by_both_revisions() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["product-1", "v1"], 64).await;
        fill(store, "/p/1", &["product-1", "v2"], 64).await;

        assert_eq!(store.purge_tags(["v1"]), Purged::default());
        assert_eq!(
            store.purge_tags(["product-1"]).entries,
            1,
            "replacing the entry removed its still-current tag from the index"
        );
    }

    #[tokio::test]
    async fn metadata_updates_replace_the_tag_index_membership() {
        let store = store_of(1 << 20);
        fill(store, "/p/1", &["old-tag"], 64).await;

        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header("x-tanod-cache-tags", "new-tag")
            .unwrap();
        let now = SystemTime::now();
        let replacement = CacheMeta::new(now + Duration::from_secs(600), now, 0, 0, header);
        assert!(
            store
                .update_meta(
                    &CacheKey::new("", "/p/1", ""),
                    &replacement,
                    &Span::inactive().handle(),
                )
                .await
                .unwrap()
        );

        assert_eq!(store.purge_tags(["old-tag"]), Purged::default());
        assert_eq!(store.purge_tags(["new-tag"]).entries, 1);
    }

    #[tokio::test]
    async fn a_tag_purge_prevents_a_matching_in_progress_fill_from_being_admitted() {
        let store = store_of(1 << 20);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header("x-tanod-cache-tags", "purged-tag")
            .unwrap();
        let now = SystemTime::now();
        let meta = CacheMeta::new(now + Duration::from_secs(600), now, 0, 0, header);
        let key = CacheKey::new("", "/in-progress", "/in-progress");
        let mut writer = store
            .get_miss_handler(&key, &meta, &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();

        assert_eq!(store.purge_tags(["purged-tag"]), Purged::default());
        writer.write_body(Bytes::new(), true).await.unwrap();
        writer.finish().await.unwrap();

        assert!(!is_key_cached(store, "/in-progress").await);
        assert_eq!(store.bytes_used(), 0);
    }

    #[tokio::test]
    async fn purge_all_prevents_old_deployment_fills_from_being_admitted_late() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/old-build", "/old-build");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();

        assert_eq!(store.purge_all(), Purged::default());
        writer.write_body(Bytes::new(), true).await.unwrap();
        writer.finish().await.unwrap();

        assert!(!is_key_cached(store, "/old-build").await);
        assert_eq!(store.bytes_used(), 0);
    }

    #[test]
    fn tag_parsing_is_bounded_on_every_axis() {
        assert_eq!(parse_tags("a, b ,c"), ["a", "b", "c"]);
        assert_eq!(parse_tags("a,,  ,a"), ["a"], "empties and repeats dropped");
        assert!(parse_tags(&"x".repeat(MAX_TAG_LEN + 1)).is_empty());
        assert_eq!(parse_tags("oké,fine").len(), 1, "non-ASCII tag dropped");
        let many = (0..MAX_TAGS_PER_ENTRY * 2)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_tags(&many).len(), MAX_TAGS_PER_ENTRY);
    }

    #[tokio::test]
    async fn a_transient_entry_is_never_tagged_or_indexed() {
        let store = store_of(1 << 20);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
        header
            .insert_header("x-tanod-cache-tags", "ghost")
            .unwrap();
        let now = SystemTime::now();
        let meta = CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header);
        let key = CacheKey::new("", "/transient", "");
        let mut writer = store
            .get_miss_handler(&key, &meta, &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"body"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert_eq!(store.tags(), 0, "a transient response was indexed");
    }

    // ------------------------------------------------------------- eviction

    /// The budget that holds exactly `entries` entries of this body size.
    ///
    /// Probed rather than guessed: the per-entry overhead is a serialised
    /// `CacheMeta`, whose size is not this test's business and would silently
    /// turn "nothing was evicted" into a passing assertion.
    async fn budget_for(entries: usize, body_len: usize) -> usize {
        let probe = store_of(1 << 20);
        fill(probe, "/probe", &[], body_len).await;
        let per_entry = probe.bytes_used();
        per_entry * entries + per_entry / 2
    }

    #[tokio::test]
    async fn clock_spares_an_entry_that_has_been_read() {
        let store = store_of(budget_for(2, 1200).await);
        fill(store, "/hot", &[], 1200).await;
        fill(store, "/cold", &[], 1200).await;
        // Reading the older entry sets its visited bit.
        assert!(is_cached(store, "/hot").await);
        // A third entry has to displace something.
        fill(store, "/new", &[], 1200).await;

        assert!(
            is_cached(store, "/hot").await,
            "second-chance FIFO evicted the entry that had just been read"
        );
        assert!(!is_cached(store, "/cold").await);
    }

    #[tokio::test]
    async fn fifo_ignores_reads_entirely() {
        let store = BoundedStore::new(&CacheDefaults {
            max_memory: ByteSize(budget_for(2, 1200).await as u64),
            eviction: Eviction::Fifo,
            ..Default::default()
        });
        fill(store, "/hot", &[], 1200).await;
        fill(store, "/cold", &[], 1200).await;
        assert!(is_cached(store, "/hot").await);
        fill(store, "/new", &[], 1200).await;
        assert!(
            !is_cached(store, "/hot").await,
            "FIFO evicts by arrival order; this is no longer a control"
        );
    }

    /// Eviction must terminate even when every entry is hot.
    ///
    /// The livelock this guards: second chance requeues a visited entry, so a
    /// working set that is entirely visited could rotate forever. The rotation
    /// is bounded, and past the bound the policy degrades to FIFO rather than
    /// spinning.
    #[tokio::test]
    async fn eviction_terminates_when_every_entry_is_hot() {
        let budget = budget_for(3, 1000).await;
        let store = store_of(budget);
        for n in 0..3 {
            fill(store, &format!("/p/{n}"), &[], 1000).await;
        }
        for n in 0..3 {
            let _ = is_cached(store, &format!("/p/{n}")).await;
        }
        // Every remaining entry is visited. This must return, not hang.
        fill(store, "/newcomer", &[], 1000).await;
        assert!(is_cached(store, "/newcomer").await);
        assert!(store.bytes_used() <= budget);
    }

    /// The measurement behind `cache.eviction` defaulting to `clock`.
    ///
    /// An SSR microcache sees a heavily skewed request distribution: a handful
    /// of URLs are most of the traffic. FIFO evicts a hot entry as readily as
    /// a cold one purely because it arrived earlier, which is exactly the
    /// wrong call on this shape of workload. This asserts the improvement
    /// rather than claiming it.
    #[tokio::test]
    async fn clock_beats_fifo_on_a_skewed_workload() {
        // Zipf-ish: rank r is drawn with weight 1/(r+1), from a deterministic
        // xorshift so the run is reproducible and both policies see the
        // identical request sequence.
        const KEYS: usize = 200;
        const REQUESTS: usize = 4_000;
        const BODY: usize = 400;
        const BUDGET: usize = 20 * (BODY + 400);

        fn workload() -> Vec<usize> {
            let mut seed = 0x2545_F491_4F6C_DD1Du64;
            let mut cumulative: Vec<f64> = Vec::with_capacity(KEYS);
            let mut total = 0.0;
            for rank in 0..KEYS {
                total += 1.0 / (rank as f64 + 1.0);
                cumulative.push(total);
            }
            (0..REQUESTS)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let unit = (seed >> 11) as f64 / (1u64 << 53) as f64;
                    let pick = unit * total;
                    cumulative.iter().position(|c| *c >= pick).unwrap_or(0)
                })
                .collect()
        }

        async fn hit_ratio(policy: Eviction, requests: &[usize]) -> f64 {
            let store = BoundedStore::new(&CacheDefaults {
                max_memory: ByteSize(BUDGET as u64),
                eviction: policy,
                ..Default::default()
            });
            let mut hits = 0usize;
            for key in requests {
                let path = format!("/p/{key}");
                if is_cached(store, &path).await {
                    hits += 1;
                } else {
                    fill(store, &path, &[], BODY).await;
                }
            }
            hits as f64 / requests.len() as f64
        }

        let requests = workload();
        let fifo = hit_ratio(Eviction::Fifo, &requests).await;
        let clock = hit_ratio(Eviction::Clock, &requests).await;
        assert!(
            clock > fifo,
            "second-chance FIFO ({clock:.3}) did not beat FIFO ({fifo:.3})"
        );
        // Measured 0.600 (clock) against 0.525 (FIFO) at these parameters — a
        // seventh more requests served without touching the origin, for one
        // relaxed atomic per hit. Asserted as a floor so a regression reads as
        // a number rather than a feeling.
        assert!(
            clock - fifo > 0.03,
            "the improvement was only {:.3} ({clock:.3} vs {fifo:.3}); the default \
             is not earning its extra atomic",
            clock - fifo
        );
    }

    // ------------------------------------------------- storage evaluation

    /// What an in-process cache hit actually costs.
    ///
    /// This exists to keep `docs/CACHE-STORAGE-EVALUATION.md` honest. The
    /// question that document answers — whether the cache should move to disk
    /// or to an external store — turns entirely on how expensive a hit is
    /// today, because every alternative adds either a syscall or a network
    /// round trip to a path whose whole job is to be cheaper than rendering.
    ///
    /// Asserted as a ceiling rather than reported, so the argument decays into
    /// a test failure rather than into a stale claim in a document. The
    /// ceiling is deliberately loose: this runs on shared CI hardware in a
    /// debug build, and the finding is "microseconds, not milliseconds", not
    /// any particular microsecond count.
    #[tokio::test]
    async fn an_in_process_hit_costs_microseconds_not_milliseconds() {
        // A realistically sized server-rendered document.
        const BODY: usize = 64 * 1024;
        const SAMPLES: u32 = 200;

        let store = store_of(8 << 20);
        fill(store, "/page", &[], BODY).await;

        // Warm once, so the measurement is of a hit rather than of first
        // touch.
        assert!(is_cached(store, "/page").await);

        let started = std::time::Instant::now();
        for _ in 0..SAMPLES {
            assert!(is_cached(store, "/page").await);
        }
        let per_hit = started.elapsed() / SAMPLES;

        assert!(
            per_hit < Duration::from_micros(500),
            "an in-process cache hit took {per_hit:?}; the storage evaluation assumes \
             microseconds, and an external store's network round trip would no longer be \
             the dominant cost"
        );
    }

    #[tokio::test]
    async fn lookup_serves_a_write_that_is_still_in_progress() {
        // The regression this guards: Pingora releases the cache lock as soon
        // as the leader's miss handler exists, so coalesced waiters arrive at
        // `lookup` with no write tag while the body is still being written.
        // A `lookup` that only answers from finished entries makes every
        // waiter miss and go to the origin — request collapsing failing
        // silently on exactly the streaming responses it matters most for.
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/streaming", "");

        let mut leader = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        leader
            .write_body(Bytes::from_static(b"shell"), false)
            .await
            .unwrap();

        let hit = store
            .lookup(&key, &Span::inactive().handle())
            .await
            .unwrap();
        let (_meta, mut reader) =
            hit.expect("a waiter must be able to attach to the in-flight write");
        assert_eq!(&reader.read_body().await.unwrap().unwrap()[..], b"shell");
    }

    #[tokio::test]
    async fn lookup_prefers_a_finished_entry_over_a_new_write() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/revalidating", "");

        let mut first = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        first
            .write_body(Bytes::from_static(b"old"), true)
            .await
            .unwrap();
        first.finish().await.unwrap();

        // A revalidation starts while the finished entry is still servable.
        let mut second = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        second
            .write_body(Bytes::from_static(b"new"), false)
            .await
            .unwrap();

        let (_m, mut reader) = store
            .lookup(&key, &Span::inactive().handle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            &reader.read_body().await.unwrap().unwrap()[..],
            b"old",
            "a complete entry should win over a half-written one"
        );
    }

    #[tokio::test]
    async fn lookup_still_misses_when_there_is_nothing_at_all() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/absent", "");
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropping_an_incomplete_fill_releases_memory_and_removes_the_temp_entry() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/abandoned", "");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();
        assert!(store.bytes_used() > 0);

        drop(writer);

        assert_eq!(store.bytes_used(), 0);
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_streaming_tag_never_falls_back_to_an_unrelated_entry() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/tag", "");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"done"), true)
            .await
            .unwrap();
        let stale_tag = writer.streaming_write_tag().unwrap().to_vec();
        writer.finish().await.unwrap();

        assert!(
            store
                .lookup_streaming_write(&key, Some(&stale_tag), &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn transient_fills_exist_only_for_the_follower_handoff() {
        let store = store_of(1 << 20);
        let key = CacheKey::new("", "/transient", "");
        let now = SystemTime::now();
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
        let transient = CacheMeta::new(now + Duration::from_secs(30), now, 0, 0, header);
        let mut writer = store
            .get_miss_handler(&key, &transient, &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"one flight"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();

        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_some()
        );
        tokio::time::sleep(TRANSIENT_HANDOFF + Duration::from_millis(25)).await;
        assert_eq!(store.bytes_used(), 0);
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn in_progress_fills_count_toward_the_global_budget() {
        let store = store_of(4096);
        let first = CacheKey::new("", "/one", "");
        let second = CacheKey::new("", "/two", "");
        let mut a = store
            .get_miss_handler(&first, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        a.write_body(Bytes::from(vec![b'a'; 3000]), false)
            .await
            .unwrap();
        let mut b = store
            .get_miss_handler(&second, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        assert!(
            b.write_body(Bytes::from(vec![b'b'; 3000]), false)
                .await
                .is_err()
        );
        assert!(store.bytes_used() <= 4096);
        drop(a);
        drop(b);
        assert_eq!(store.bytes_used(), 0);
    }
}
