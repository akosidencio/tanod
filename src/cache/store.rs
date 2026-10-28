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
//!
//! # Memory exhaustion
//!
//! Running out of budget must not turn a good origin response into a failed
//! request where that can be avoided, and where it cannot, it must fail
//! visibly rather than truncate silently. What is possible depends on where in
//! Pingora's lifecycle the budget runs out:
//!
//! * **Registering a fill** (the metadata reservation in `get_miss_handler`).
//!   Refused, and Pingora, which has sent nothing downstream yet, disables the
//!   cache for that response and serves it uncached. The proxy checks
//!   [`BoundedStore::accepting_fills`] first so the usual case is a clean
//!   uncacheable decision rather than a storage error.
//! * **Writing the body.** By now Pingora serves the client *from this store*
//!   and gives up the whole request if a write fails. So a fill that cannot
//!   reserve its next chunk is *abandoned* instead: it will never be stored,
//!   and it keeps streaming to the readers it has, accounted against a bounded
//!   headroom of one `cache.max_body_size` above the budget. Only when that
//!   headroom is exhausted too does the write fail, and Pingora aborts the
//!   response rather than presenting a truncated body as complete.
//!
//! Every reserved byte is released exactly once: by completion, by the
//! handoff timer, or by the handler's drop, whichever owns it.

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use pingora_cache::CacheMeta;
use pingora_cache::key::{CacheHashKey, CacheKey};
use pingora_cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeOutcome, PurgeTarget,
    PurgeType, Storage, streaming_write::U64WriteId,
};
use pingora_cache::trace::SpanHandle;
use pingora_error::{Error, ErrorType, Result};
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::watch;

use super::eviction::{TinyPolicy, Victim};
use super::key::CacheNamespace;
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

/// How a fill's bytes were accounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    /// Within `cache.max_memory`.
    Budget,
    /// Above it, within the abandoned-fill headroom.
    Headroom,
    /// Neither. Nothing was reserved and nothing was evicted.
    Refused,
}

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

/// The most one `read_body` call returns, from a finished entry or a fill.
///
/// Pingora writes each returned chunk downstream as one write, and
/// `timeouts.downstream_write` bounds each write, so this is also what a slow
/// reader has to accept within that timeout. Unchanged from when bodies were
/// copied out in slices of this size.
const READ_CHUNK: usize = 64 * 1024;

/// A body write at least this large is kept as its own immutable segment.
/// Smaller writes accumulate until they reach it, so a stream of tiny chunks
/// costs one segment per this many bytes rather than one per write.
const SEAL_AT: usize = 4 * 1024;

/// The body of a fill in progress.
///
/// Readers — the leader's own response and every coalesced follower — share
/// sealed segments by reference count rather than copying them. Only the
/// unsealed tail, under [`SEAL_AT`] bytes, is copied out, so following a fill
/// costs no more than a few kilobytes of copying per reader however large the
/// body.
///
/// Append-only: a segment never changes or moves once sealed, which is what
/// lets a reader keep its position as a segment index across calls.
#[derive(Default)]
struct FillBody {
    sealed: Vec<Bytes>,
    sealed_len: usize,
    /// Writes not yet sealed, fewer than [`SEAL_AT`] bytes.
    open: Vec<u8>,
}

/// A reader's position in a body: a byte offset, plus the segment that holds
/// it so a read does not rescan from the start.
#[derive(Default)]
struct Cursor {
    offset: usize,
    segment: usize,
    segment_start: usize,
}

impl Cursor {
    fn at(offset: usize) -> Self {
        Cursor {
            offset,
            ..Cursor::default()
        }
    }

    /// Up to `limit` bytes from the sealed segments at this position, without
    /// copying, or `None` once the position is past every sealed segment.
    fn slice(&mut self, segments: &[Bytes], limit: usize) -> Option<Bytes> {
        while let Some(segment) = segments.get(self.segment) {
            let from = self.offset.checked_sub(self.segment_start)?;
            if from < segment.len() {
                let chunk = segment.slice(from..segment.len().min(from + limit));
                self.offset += chunk.len();
                return Some(chunk);
            }
            self.segment_start += segment.len();
            self.segment += 1;
        }
        None
    }
}

impl FillBody {
    fn len(&self) -> usize {
        self.sealed_len + self.open.len()
    }

    /// Append one write. `owned` is `data` already copied into its own exact
    /// allocation — done by the caller before taking the body lock — when the
    /// write is large enough to become a segment by itself.
    ///
    /// Always a copy, never the caller's `Bytes`: an upstream chunk can be a
    /// slice of a larger receive buffer (HTTP/2 frames are), and retaining it
    /// would keep that whole buffer alive behind a few accounted bytes.
    fn append(&mut self, data: &[u8], owned: Option<Bytes>) {
        match owned {
            Some(segment) => {
                self.seal_open();
                self.sealed_len += segment.len();
                self.sealed.push(segment);
            }
            None => {
                self.open.extend_from_slice(data);
                if self.open.len() >= SEAL_AT {
                    self.seal_open();
                }
            }
        }
    }

    fn seal_open(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let segment = Bytes::copy_from_slice(&self.open);
        self.open.clear();
        self.sealed_len += segment.len();
        self.sealed.push(segment);
    }

    /// The next chunk at `cursor`, ending no later than `available`: what the
    /// writer had published when the reader looked. Sealed bytes are shared;
    /// tail bytes are copied.
    fn read(&self, cursor: &mut Cursor, available: usize) -> Option<Bytes> {
        let available = available.min(self.len());
        if cursor.offset >= available {
            return None;
        }
        let limit = READ_CHUNK.min(available - cursor.offset);
        if let Some(chunk) = cursor.slice(&self.sealed, limit) {
            return Some(chunk);
        }
        let from = cursor.offset.checked_sub(self.sealed_len)?;
        let tail = self.open.get(from..from + limit)?;
        cursor.offset += tail.len();
        Some(Bytes::copy_from_slice(tail))
    }

    /// The finished body, for storage. Seals the tail first; followers still
    /// reading this fill keep their positions, because sealing moves bytes
    /// from the tail into a segment without changing any offset.
    ///
    /// A body whose segments would make a hit take more downstream writes
    /// than a contiguous body is compacted into [`READ_CHUNK`]-sized segments,
    /// once, here. One copy per fill buys every later hit the fewest writes:
    /// a 16 KiB page that arrived in two upstream reads is otherwise two
    /// writes on every hit, which measured slower than the copy it saves. A
    /// body already laid out for the fewest writes is shared with no copy.
    fn freeze(&mut self) -> Frozen {
        self.seal_open();
        let len = self.sealed_len;
        let fewest = len.div_ceil(READ_CHUNK);
        let writes: usize = self
            .sealed
            .iter()
            .map(|s| s.len().div_ceil(READ_CHUNK))
            .sum();
        if writes <= fewest {
            return Frozen {
                segments: self.sealed.clone(),
                len,
            };
        }
        let mut segments = Vec::with_capacity(fewest);
        let mut remaining = len;
        // Never a zero capacity: the copy below advances by the room left.
        let mut current = Vec::with_capacity(READ_CHUNK.min(remaining).max(1));
        for segment in &self.sealed {
            let mut rest = segment.as_ref();
            while !rest.is_empty() {
                let take = (current.capacity() - current.len()).min(rest.len());
                let (head, tail) = rest.split_at(take);
                current.extend_from_slice(head);
                rest = tail;
                if current.len() == current.capacity() {
                    remaining = remaining.saturating_sub(current.len());
                    segments.push(Bytes::from(std::mem::replace(
                        &mut current,
                        Vec::with_capacity(READ_CHUNK.min(remaining).max(1)),
                    )));
                }
            }
        }
        if !current.is_empty() {
            segments.push(Bytes::from(current));
        }
        Frozen { segments, len }
    }
}

/// A finished body. Immutable, so hits read it with no lock at all.
struct Frozen {
    segments: Vec<Bytes>,
    len: usize,
}

/// A finished entry.
struct Stored {
    meta: Arc<BinaryMeta>,
    body: Arc<Frozen>,
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
    /// Distinguishes this admission from earlier ones under the same key, so
    /// a retired queue record cannot evict a replacement: TinyUFO's policy
    /// revision, or under CLOCK and FIFO the [`EvictionQueue`] ticket. The
    /// shared hash avoids allocating a lookup key on every TinyUFO hit.
    revision: u64,
    tiny_hash: Option<Arc<str>>,
}

impl Stored {
    fn size(&self) -> usize {
        self.meta.0.len()
            + self.meta.1.len()
            + self.body.len
            + self.path.len()
            + self.tags.iter().map(String::len).sum::<usize>()
    }
}

/// A write in progress. Readers attach to this and follow it.
struct Temp {
    meta: Arc<BinaryMeta>,
    body: Arc<RwLock<FillBody>>,
    written: Arc<watch::Sender<Written>>,
    transient: bool,
    tags: Vec<String>,
    path: String,
    /// A purge matched this fill while it was in progress. Existing followers
    /// may still drain the body, but it must not be admitted afterward.
    invalidated: bool,
    namespace: Option<CacheNamespace>,
    /// Pingora's leader is a reader too. Reserve its attachment at registration:
    /// set_miss_handler panics if its subsequent exact-tag lookup misses. The
    /// private writer tag transfers this reader once, even if purge intervenes;
    /// it never authorizes a new attachment to an invalidated response.
    leader: Option<watch::Receiver<Written>>,
}

impl Temp {
    fn new(meta: BinaryMeta, transient: bool, tags: Vec<String>, path: String) -> Self {
        let (tx, rx) = watch::channel(Written::Partial(0));
        Temp {
            meta: Arc::new(meta),
            body: Arc::new(RwLock::new(FillBody::default())),
            written: Arc::new(tx),
            transient,
            tags,
            path,
            invalidated: false,
            namespace: None,
            leader: Some(rx),
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
    /// Contains the namespace allowed to admit completed entries. Old pinned
    /// requests may stream in their own namespace, but cannot retain entries.
    /// The read side is held only for those short state transitions, never
    /// while response bytes stream.
    lifecycle: RwLock<Option<CacheNamespace>>,
    cached: RwLock<HashMap<String, Stored>>,
    /// Eviction queue. Front is the next candidate; under
    /// [`Eviction::Clock`] an entry that has been read gets requeued at the
    /// back instead of being discarded.
    order: RwLock<EvictionQueue>,
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
    /// The part of `used` held by fills: in progress, in a handoff, or
    /// abandoned. Unlike completed entries these cannot be evicted, so a
    /// reservation that would not fit beside them is refused up front rather
    /// than after emptying the cache for nothing. Written only while `used`
    /// is held; an atomic so status and metrics can read it without the lock.
    pinned: AtomicUsize,
    max_bytes: usize,
    /// How far abandoned fills may take `pinned` above `max_bytes` so the
    /// responses already streaming from them can finish. One maximum body:
    /// whatever else happens, the fill that runs out first can complete.
    headroom: usize,
    next_id: AtomicU64,
    policy: Eviction,
    tiny: Option<RwLock<TinyPolicy>>,
    /// Lowercased once at startup so the per-response lookup is a plain
    /// `HeaderMap::get`.
    tag_header: String,
    /// Entries discarded to make room, and entries discarded by an explicit
    /// purge. Separate counters: one is the cache working, the other is
    /// somebody invalidating, and confusing them makes a dashboard lie.
    evicted: AtomicU64,
    purged: AtomicU64,
    admission_rejected: AtomicU64,
    /// Memory-pressure outcomes; see the module documentation.
    uncached_fallbacks: AtomicU64,
    fills_abandoned: AtomicU64,
    fills_exhausted: AtomicU64,
}

/// Memory-pressure counters, for the status document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryPressure {
    /// Responses served uncached because a fill could not be registered.
    pub uncached_fallback: u64,
    /// Fills that ran out mid-body and finished from the headroom, unstored.
    pub fill_abandoned: u64,
    /// Fills that ran out of headroom too; their responses were aborted.
    pub memory_exhausted: u64,
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
            lifecycle: RwLock::new(None),
            cached: RwLock::new(HashMap::new()),
            order: RwLock::new(EvictionQueue::default()),
            tag_index: RwLock::new(HashMap::new()),
            temp: RwLock::new(HashMap::new()),
            used: Mutex::new(0),
            pinned: AtomicUsize::new(0),
            max_bytes: cfg.max_memory.as_usize(),
            // Validation already keeps `max_body_size` within `max_memory`;
            // the clamp keeps the bound at most twice the budget regardless.
            headroom: cfg.max_body_size.as_usize().min(cfg.max_memory.as_usize()),
            next_id: AtomicU64::new(0),
            policy: cfg.eviction,
            tiny: (cfg.eviction == Eviction::Tinyufo)
                .then(|| RwLock::new(TinyPolicy::new(cfg.max_memory.as_usize()))),
            tag_header: cfg.tag_header.to_ascii_lowercase(),
            evicted: AtomicU64::new(0),
            purged: AtomicU64::new(0),
            admission_rejected: AtomicU64::new(0),
            uncached_fallbacks: AtomicU64::new(0),
            fills_abandoned: AtomicU64::new(0),
            fills_exhausted: AtomicU64::new(0),
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

    pub fn admission_rejected(&self) -> u64 {
        self.admission_rejected.load(Ordering::Relaxed)
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

    /// Bytes held by fills rather than completed entries.
    pub fn bytes_pinned(&self) -> usize {
        self.pinned.load(Ordering::Relaxed)
    }

    /// How far abandoned fills may exceed [`limit`](Self::limit).
    pub fn headroom(&self) -> usize {
        self.headroom
    }

    pub fn memory_pressure(&self) -> MemoryPressure {
        MemoryPressure {
            uncached_fallback: self.uncached_fallbacks.load(Ordering::Relaxed),
            fill_abandoned: self.fills_abandoned.load(Ordering::Relaxed),
            memory_exhausted: self.fills_exhausted.load(Ordering::Relaxed),
        }
    }

    /// Could a new fill of about `estimate` bytes of metadata register now?
    ///
    /// Advisory: the proxy asks before offering Pingora a cacheable response,
    /// so that the usual exhaustion case becomes an uncacheable response
    /// instead of a storage error. A fill that passes this and then loses a
    /// race is still refused safely by `get_miss_handler`.
    pub fn accepting_fills(&self, estimate: usize) -> bool {
        self.pinned.load(Ordering::Relaxed).saturating_add(estimate) <= self.max_bytes
    }

    /// Record a response the proxy served uncached because the cache was full.
    pub fn note_uncached_fallback(&self) {
        self.uncached_fallbacks.fetch_add(1, Ordering::Relaxed);
        crate::telemetry::metrics::CACHE_MEMORY_PRESSURE
            .with_label_values(&["uncached_fallback"])
            .inc();
    }

    /// Reserve `additional` bytes for a fill, evicting completed entries as
    /// needed.
    ///
    /// `budget` allows the reservation inside `max_bytes`; `headroom` allows
    /// it inside `max_bytes + headroom`, which only an abandoned fill may use.
    /// Feasibility is decided from `pinned` before anything is evicted:
    /// completed entries can always be evicted, fills cannot, so a
    /// reservation that cannot fit beside the fills is refused without
    /// emptying the cache first.
    ///
    /// Accounting transitions lock `used` → `cached` → `order` →
    /// `tag_index` → `tiny`, skipping unused locks. Lookup takes only cache
    /// and policy read locks. Never acquire the cache map after the policy
    /// lock: readers and eviction must observe the same entry identity.
    ///
    /// A reservation that fits without evicting takes `used` alone. The map
    /// locks are only for choosing and removing victims, and taking them for
    /// every body chunk of every fill made each chunk wait out — and block —
    /// every concurrent lookup's read of the cache map.
    fn reserve_fill(&self, additional: usize, budget: bool, headroom: bool) -> Grant {
        let mut used = self.used.lock();
        let pinned = self.pinned.load(Ordering::Relaxed);
        let wanted = pinned.saturating_add(additional);
        let (cap, grant) = if budget && wanted <= self.max_bytes {
            (self.max_bytes, Grant::Budget)
        } else if headroom && wanted <= self.max_bytes.saturating_add(self.headroom) {
            (
                self.max_bytes.saturating_add(self.headroom),
                Grant::Headroom,
            )
        } else {
            return Grant::Refused;
        };
        // Exactly what the eviction loop below would conclude after zero
        // iterations; `used` is held throughout, so nothing can change it in
        // between.
        if used.saturating_add(additional) <= cap {
            *used += additional;
            self.pinned.fetch_add(additional, Ordering::Relaxed);
            return grant;
        }
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
            Eviction::Clock => order.records.len().saturating_add(1),
            Eviction::Fifo | Eviction::Tinyufo => 0,
        };
        let mut tiny = self.tiny.as_ref().map(|policy| policy.write());
        while used.saturating_add(additional) > cap {
            if let Some(policy) = tiny.as_mut() {
                let victims = policy.evict();
                if victims.is_empty() {
                    return Grant::Refused;
                }
                self.discard_victims(victims, &mut used, &mut cached, &mut tag_index);
                Self::compact(policy, &cached);
                continue;
            }
            let Some((victim, ticket)) = order.records.pop_front() else {
                return Grant::Refused;
            };
            // A record whose entry was replaced or purged since it was queued
            // is retired here rather than searched for at the time.
            let Some(entry) = cached.get(&victim).filter(|entry| entry.revision == ticket) else {
                continue;
            };
            if chances > 0 && entry.visited.swap(false, Ordering::Relaxed) {
                chances -= 1;
                order.records.push_back((victim, ticket));
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
        self.pinned.fetch_add(additional, Ordering::Relaxed);
        grant
    }

    /// Return a fill's whole reservation. Called exactly once per fill, by
    /// whichever of completion, handoff expiry or drop owns it.
    fn release(&self, amount: usize) {
        let mut used = self.used.lock();
        *used = used.saturating_sub(amount);
        let pinned = self.pinned.load(Ordering::Relaxed);
        debug_assert!(
            pinned >= amount,
            "released {amount} of {pinned} pinned bytes"
        );
        self.pinned
            .store(pinned.saturating_sub(amount), Ordering::Relaxed);
    }

    /// Convert an already-accounted fill. Return an admission-rejected object
    /// to its streaming handoff; its bytes remain reserved until handoff ends.
    /// An admitted fill's `accounted` bytes stop being pinned: they are now a
    /// completed entry, which eviction may take.
    fn admit(&self, key: String, mut obj: Stored, accounted: usize) -> Option<Stored> {
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        let mut tag_index = self.tag_index.write();
        let mut tiny = self.tiny.as_ref().map(|policy| policy.write());
        if let Some(old) = cached.remove(&key) {
            *used = used.saturating_sub(old.size());
            unindex_tags(&mut tag_index, &key, &old.tags);
            // Under CLOCK and FIFO the old entry's queue record is left to be
            // retired when it reaches the front: removing it here meant a
            // scan of the whole queue, under every cache write lock, on every
            // replacement — every expiry of a microcached page.
            if let Some(policy) = tiny.as_mut() {
                policy.remove(&key, old.revision);
                policy.invalidate();
            }
        }
        if let Some(policy) = tiny.as_mut() {
            obj.revision = policy.revision();
            obj.tiny_hash = Some(Arc::from(key.as_str()));
            let victims = policy.admit(&key, obj.size());
            let rejected = victims
                .iter()
                .any(|v| v.hash.as_ref() == key && v.revision == obj.revision);
            self.discard_victims(victims, &mut used, &mut cached, &mut tag_index);
            if rejected {
                self.admission_rejected.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::metrics::CACHE_ADMISSION_REJECTED.inc();
                Self::compact(policy, &cached);
                return Some(obj);
            }
        }
        for tag in &obj.tags {
            tag_index
                .entry(tag.clone())
                .or_default()
                .insert(key.clone());
        }
        if tiny.is_none() {
            obj.revision = order.ticket();
            order.records.push_back((key.clone(), obj.revision));
        }
        cached.insert(key, obj);
        let pinned = self.pinned.load(Ordering::Relaxed);
        self.pinned
            .store(pinned.saturating_sub(accounted), Ordering::Relaxed);
        if let Some(policy) = tiny.as_mut() {
            Self::compact(policy, &cached);
        } else {
            order.compact_if_needed(&cached);
        }
        None
    }

    fn compact(policy: &mut TinyPolicy, cached: &HashMap<String, Stored>) {
        policy.compact_if_needed(
            cached.iter().map(|(hash, entry)| {
                (
                    hash.as_str(),
                    entry.revision,
                    entry.size(),
                    entry.visited.load(Ordering::Relaxed),
                )
            }),
            cached.len(),
        );
    }

    fn discard_victims(
        &self,
        victims: Vec<Victim>,
        used: &mut usize,
        cached: &mut HashMap<String, Stored>,
        tag_index: &mut HashMap<String, HashSet<String>>,
    ) {
        for victim in victims {
            if cached
                .get(victim.hash.as_ref())
                .is_some_and(|entry| entry.revision == victim.revision)
                && let Some(entry) = cached.remove(victim.hash.as_ref())
            {
                *used = used.saturating_sub(entry.size());
                unindex_tags(tag_index, victim.hash.as_ref(), &entry.tags);
                self.evicted.fetch_add(1, Ordering::Relaxed);
                crate::telemetry::metrics::CACHE_EVICTED.inc();
            }
        }
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

    /// Invalidate all registered fills and drop every completed entry.
    /// Existing followers retain their body and progress notification Arcs.
    pub fn purge_all(&self) -> Purged {
        let _lifecycle = self.lifecycle.write();
        self.purge_all_locked()
    }

    /// Rotate admission before publishing the new policy. The lifecycle write
    /// guard makes retirement, invalidation and completed-entry reclamation one
    /// transition against writer registration/completion. No namespace history
    /// is retained, so rapid reloads cannot grow a retired-generation registry.
    pub(crate) fn activate_namespace(&self, namespace: u64) -> Purged {
        let mut active = self.lifecycle.write();
        let next = Some(CacheNamespace(namespace));
        if *active == next {
            return Purged::default();
        }
        *active = next;
        self.purge_all_locked()
    }

    fn purge_all_locked(&self) -> Purged {
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
        let mut tiny = self.tiny.as_ref().map(|policy| policy.write());
        if !doomed.is_empty()
            && let Some(policy) = tiny.as_mut()
        {
            policy.invalidate();
        }
        let mut purged = Purged::default();
        for key in &doomed {
            if let Some(entry) = cached.remove(key) {
                let size = entry.size();
                *used = used.saturating_sub(size);
                purged.entries += 1;
                purged.bytes += size;
                unindex_tags(&mut tag_index, key, &entry.tags);
                if let Some(policy) = tiny.as_mut() {
                    policy.remove(key, entry.revision);
                }
            }
        }
        if let Some(policy) = tiny.as_mut() {
            Self::compact(policy, &cached);
        }
        if !doomed.is_empty() {
            order.compact_if_needed(&cached);
            self.purged
                .fetch_add(purged.entries as u64, Ordering::Relaxed);
        }
        purged
    }

    /// A hit on a finished entry, if there is one. The caller holds the
    /// read side of `lifecycle`.
    ///
    /// Recording the read here rather than in the hit handler is deliberate:
    /// this is the point at which the entry demonstrably had value, whether
    /// or not the client goes on to read the whole body.
    fn completed(&self, hash: &str) -> Result<Option<(CacheMeta, HitHandler)>> {
        let found = {
            let cached = self.cached.read();
            cached.get(hash).map(|obj| {
                obj.visited.store(true, Ordering::Relaxed);
                if let (Some(policy), Some(hash)) = (&self.tiny, &obj.tiny_hash) {
                    policy.read().hit(hash, obj.revision);
                }
                (obj.meta.clone(), obj.body.clone())
            })
        };
        let Some((meta, body)) = found else {
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(CompleteHit {
                body,
                cursor: Cursor::default(),
            }),
        )))
    }

    /// Stop new readers attaching to one fill. Its existing readers keep
    /// what they have; see `Temp::invalidated`.
    fn invalidate_fill(&self, key: &str, id: u64) {
        if let Some(fill) = self
            .temp
            .write()
            .get_mut(key)
            .and_then(|writes| writes.get_mut(&id))
        {
            fill.invalidated = true;
        }
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

/// The CLOCK/FIFO eviction queue: one record per stored entry, plus records
/// for entries since replaced or purged, which are skipped when they reach
/// the front.
///
/// A record names its entry by key and ticket. The ticket is the entry's
/// `revision`, unique per admission, so a replacement under the same key is
/// never mistaken for the entry an older record was queued for. Victims are
/// chosen in exactly the order an eagerly cleaned queue would give.
#[derive(Default)]
struct EvictionQueue {
    records: VecDeque<(String, u64)>,
    next_ticket: u64,
}

impl EvictionQueue {
    fn ticket(&mut self) -> u64 {
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.next_ticket
    }

    /// Drop retired records once they outnumber live entries, so a cache
    /// that is replaced or purged faster than it evicts cannot grow its queue
    /// without bound. Amortized: each rebuild is paid for by the retirements
    /// that made it necessary.
    fn compact_if_needed(&mut self, cached: &HashMap<String, Stored>) {
        if self.records.len() <= cached.len().saturating_mul(2).saturating_add(64) {
            return;
        }
        self.records.retain(|(key, ticket)| {
            cached
                .get(key)
                .is_some_and(|entry| entry.revision == *ticket)
        });
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
        // Attachment is atomic with purge and completion/handoff transitions.
        let _lifecycle = self.lifecycle.read();
        let hash = key.combined();

        if let Some(hit) = self.completed(&hash)? {
            return Ok(Some(hit));
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
        // Eligibility check and subscription share the temp read guard. Purge
        // marks fills under its write guard: subscribing here is attachment's
        // linearization point. Purge linearizes when its lifecycle write guard
        // is released, after all matching temps and completed entries change.
        let partial = {
            let temp = self.temp.read();
            temp.get(&hash).and_then(|writes| {
                // One leader per key in practice; take the newest if a
                // revalidation overlaps.
                writes
                    .iter()
                    .filter(|(_, fill)| !fill.invalidated)
                    .max_by_key(|(id, _)| **id)
                    .map(|(_, t)| (t.meta.clone(), t.body.clone(), t.written.subscribe()))
            })
        };
        let Some((meta, body, written)) = partial else {
            // Completion does not exclude lookups: it admits the entry to
            // `cached` and only then removes the fill from `temp`. A lookup
            // can therefore miss `cached` just before the admission and miss
            // `temp` just after the removal. Missing `temp` means any such
            // completion has already admitted, so a second look at `cached`
            // finds the entry unless it was genuinely evicted or purged. Only
            // a full miss pays for it.
            return self.completed(&hash);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(PartialHit {
                body,
                written,
                cursor: Cursor::default(),
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
        let _lifecycle = self.lifecycle.read();
        let hash = key.combined();

        // Take everything needed out of the lock in one scope: a parking_lot
        // guard is !Send, so holding one across the await below makes the
        // whole future non-Send.
        let found = {
            let mut guard = self.temp.write();
            guard
                .get_mut(&hash)
                .and_then(|m| m.get_mut(&id))
                .and_then(|temp| {
                    // This capability belongs to the miss handler, not to later
                    // requests. Transfer its pre-purge reservation first; all new
                    // subscriptions still require an eligible fill under this lock.
                    let reader = temp
                        .leader
                        .take()
                        .or_else(|| (!temp.invalidated).then(|| temp.written.subscribe()))?;
                    Some((temp.meta.clone(), temp.body.clone(), reader))
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
                cursor: Cursor::default(),
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
        // Transient responses also need purge identity: a path/tag purge must
        // hide their in-progress and scheduler-handoff forms from new readers.
        // Only completed admission populates the reverse tag index.
        let tags = meta
            .response_header()
            .headers
            .get(&self.tag_header)
            .and_then(|value| value.to_str().ok())
            .map(parse_tags)
            .unwrap_or_default();
        let path = key.user_tag().to_string();
        let serialized = meta.serialize()?;
        let tags_size: usize = tags.iter().map(String::len).sum();
        let meta_size = serialized.0.len() + serialized.1.len() + tags_size + path.len();
        if self.reserve_fill(meta_size, true, false) == Grant::Refused {
            // Nothing has been sent downstream yet, so Pingora disables the
            // cache for this response and serves it straight from the origin.
            // Headroom is not offered here: it exists to finish responses
            // already streaming, not to start new fills over budget.
            self.note_uncached_fallback();
            return Error::e_explain(ErrorType::InternalError, "cache memory budget exhausted");
        }
        let mut temp = Temp::new(serialized, transient, tags, path);
        temp.namespace = key.extensions.get::<CacheNamespace>().copied();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handler = FollowableMiss {
            store: self,
            key: hash.clone(),
            id: id.into(),
            body: temp.body.clone(),
            written: temp.written.clone(),
            accounted: meta_size,
            finished: false,
            abandoned: false,
        };
        self.temp.write().entry(hash).or_default().insert(id, temp);
        Ok(Box::new(handler))
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        _p: PurgeType,
        _t: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        // This store keeps one entry per key and assigns no entry IDs, so an
        // active and an exact target name the same entry.
        let key = target.key();
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
        Ok(if temp_invalidated || completed {
            PurgeOutcome::Purged(None)
        } else {
            PurgeOutcome::NotFound
        })
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
        obj.meta = Arc::new(replacement);
        obj.tags = replacement_tags;
        let mut tiny = self.tiny.as_ref().map(|policy| policy.write());
        if let Some(policy) = tiny.as_mut() {
            policy.remove(&hash, obj.revision);
            policy.invalidate();
            obj.revision = policy.revision();
            obj.tiny_hash = Some(Arc::from(hash.as_str()));
            let victims = policy.admit(&hash, obj.size());
            self.discard_victims(victims, &mut used, &mut cached, &mut tag_index);
            Self::compact(policy, &cached);
        }
        Ok(cached.contains_key(&hash))
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}

/// Reads a finished entry: slices of an immutable body, never a copy.
struct CompleteHit {
    body: Arc<Frozen>,
    cursor: Cursor,
}

#[async_trait]
impl HandleHit for CompleteHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        Ok(self.cursor.slice(&self.body.segments, READ_CHUNK))
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
        self.cursor = Cursor::at(start.min(self.body.len));
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
    body: Arc<RwLock<FillBody>>,
    written: watch::Receiver<Written>,
    cursor: Cursor,
}

#[async_trait]
impl HandleHit for PartialHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        loop {
            let state = *self.written.borrow_and_update();
            // Taken and released before awaiting: the guard is held only
            // while a segment is sliced or a tail of at most `SEAL_AT` bytes
            // is copied.
            if let Some(chunk) = self.body.read().read(&mut self.cursor, state.bytes()) {
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
    body: Arc<RwLock<FillBody>>,
    written: Arc<watch::Sender<Written>>,
    accounted: usize,
    finished: bool,
    /// Ran out of budget mid-body and continued on the headroom. Never
    /// stored; still served to the readers it has and any that attach.
    abandoned: bool,
}

#[async_trait]
impl HandleMiss for FollowableMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        if !data.is_empty() {
            match self.store.reserve_fill(data.len(), !self.abandoned, true) {
                Grant::Budget => {}
                Grant::Headroom => {
                    if !self.abandoned {
                        self.abandoned = true;
                        self.store.fills_abandoned.fetch_add(1, Ordering::Relaxed);
                        crate::telemetry::metrics::CACHE_MEMORY_PRESSURE
                            .with_label_values(&["fill_abandoned"])
                            .inc();
                    }
                }
                Grant::Refused => {
                    // Pingora gives up the whole request on this error: the
                    // client sees an aborted response, never a short one
                    // presented as complete. Hide the fill from new readers
                    // now; the drop that follows releases its reservation and
                    // ends the readers it has with an error.
                    self.store.invalidate_fill(&self.key, self.id.into());
                    self.store.fills_exhausted.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::metrics::CACHE_MEMORY_PRESSURE
                        .with_label_values(&["memory_exhausted"])
                        .inc();
                    return Error::e_explain(
                        ErrorType::InternalError,
                        "cache memory budget and headroom exhausted during fill",
                    );
                }
            }
        }
        self.accounted += data.len();
        // A chunk large enough to be its own segment is copied before the
        // body lock is taken, so no reader waits on that copy.
        let owned = (data.len() >= SEAL_AT).then(|| Bytes::copy_from_slice(&data));
        let total = {
            let mut body = self.body.write();
            body.append(&data, owned);
            body.len()
        };
        self.written.send_replace(if eof {
            Written::Complete(total)
        } else {
            Written::Partial(total)
        });
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<MissFinishType> {
        let id: u64 = self.id.into();
        // Seal the body and make sure any follower that has not drained yet
        // sees the end. Sealing moves no byte to a new offset, so followers
        // keep their positions.
        let frozen = self.body.write().freeze();
        let size = frozen.len;
        self.written.send_replace(Written::Complete(size));

        // The read side of `lifecycle` excludes purge and namespace
        // retirement for the whole transition: a fill a purge has marked is
        // never admitted, and a purge never sees one half admitted. Lookups
        // share the read side and are not held up by it. They cannot observe
        // a gap either: the fill stays attachable in `temp` until `cached`
        // holds the entry, and a lookup that misses `temp` looks at `cached`
        // again (see `lookup`). Hold this only for the transition; never
        // while streaming bytes or awaiting work.
        let active = self.store.lifecycle.read();
        let completion = {
            let mut temp = self.store.temp.write();
            let Some(fill) = temp.get_mut(&self.key).and_then(|w| w.get_mut(&id)) else {
                return Err(Error::explain(ErrorType::InternalError, "write vanished"));
            };
            // A finished writer no longer needs its unused leader reservation.
            fill.leader = None;
            let retired = fill.namespace.is_some() && *active != fill.namespace;
            // An abandoned fill skips the transient handoff too: its bytes are
            // over budget, and returning them now matters more than sparing a
            // late follower a render.
            if fill.invalidated || retired || self.abandoned {
                Completion::Discard
            } else if fill.transient {
                Completion::Handoff
            } else {
                // Purge cannot read this fill's tags or path while `lifecycle`
                // is held, so the entry can take them; a rejection below puts
                // them back.
                Completion::Admit(Stored {
                    meta: fill.meta.clone(),
                    body: Arc::new(frozen),
                    tags: std::mem::take(&mut fill.tags),
                    path: std::mem::take(&mut fill.path),
                    // A freshly stored entry starts unvisited. The request
                    // that filled it does not count as a read: one arrival is
                    // exactly the evidence FIFO already has, and crediting it
                    // here would give every entry a free pass and turn the
                    // policy back into FIFO with extra steps.
                    visited: AtomicBool::new(false),
                    revision: 0,
                    tiny_hash: None,
                })
            }
        };
        self.finished = true;
        match completion {
            Completion::Discard => {
                self.store.remove_temp(&self.key, id);
                self.store.release(self.accounted);
            }
            // Already in `temp`, where the late followers look for it.
            Completion::Handoff => self.schedule_handoff(id),
            Completion::Admit(stored) => {
                match self.store.admit(self.key.clone(), stored, self.accounted) {
                    None => {
                        self.store.remove_temp(&self.key, id);
                    }
                    // TinyUFO declined it. What is already streaming finishes
                    // from the fill, which becomes a transient handoff and
                    // keeps its bytes reserved until the handoff ends.
                    Some(rejected) => {
                        if let Some(fill) = self
                            .store
                            .temp
                            .write()
                            .get_mut(&self.key)
                            .and_then(|w| w.get_mut(&id))
                        {
                            fill.transient = true;
                            fill.tags = rejected.tags;
                            fill.path = rejected.path;
                        }
                        self.schedule_handoff(id);
                    }
                }
            }
        }
        drop(active);
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.id.as_bytes())
    }
}

/// What finishing a fill does with it.
enum Completion {
    /// Purged, retired or abandoned: never stored, reservation released now.
    Discard,
    /// Transient: kept briefly for the cache-lock waiters already woken.
    Handoff,
    Admit(Stored),
}

impl FollowableMiss {
    /// Keep a finished fill attachable for [`TRANSIENT_HANDOFF`], then
    /// release it. Its reservation is returned by whichever of this timer or
    /// a competing removal takes the fill out of `temp`.
    fn schedule_handoff(&self, id: u64) {
        let store = self.store;
        let key = self.key.clone();
        let accounted = self.accounted;
        tokio::spawn(async move {
            tokio::time::sleep(TRANSIENT_HANDOFF).await;
            if store.remove_temp(&key, id).is_some() {
                store.release(accounted);
            }
        });
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
#[path = "store_phase1_tests.rs"]
mod phase1_tests;

#[cfg(test)]
#[path = "store_phase2_tests.rs"]
mod phase2_tests;

#[cfg(test)]
#[path = "store_phase3_tests.rs"]
mod phase3_tests;

#[cfg(test)]
mod tests {
    use super::*;
    // Aliased: `Bytes` in this file already means `bytes::Bytes`, and the
    // body-writing tests below use it.
    use crate::config::units::Bytes as ByteSize;

    pub(super) fn cache_cfg(max_bytes: usize) -> CacheDefaults {
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

    pub(super) fn meta() -> CacheMeta {
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
        let key = CacheKey::new(path, path);
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
            .get_miss_handler(&CacheKey::new(key, path), &meta, &Span::inactive().handle())
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
            .lookup(&CacheKey::new(key, ""), &Span::inactive().handle())
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
        let bare = CacheKey::new("/canonical-string", "");
        let tagged = CacheKey::new("/canonical-string", "/products/iphone");
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
        let key = CacheKey::new("path-fill", "/purged-path");
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

    /// Pingora 0.9's purge hook, reached through its own cache API rather
    /// than Tanod's endpoint: both kinds of target must remove the entry and
    /// its tag index, and a second purge must say there was nothing there.
    #[tokio::test]
    async fn pingoras_purge_hook_removes_an_entry_for_either_kind_of_target() {
        use pingora_cache::CacheEntryKey;
        let store = store_of(1 << 20);
        let span = Span::inactive().handle();

        fill(store, "/active", &["tag-a"], 16).await;
        let active = CacheKey::new("/active", "/active").to_compact();
        let outcome = store
            .purge(PurgeTarget::Active(&active), PurgeType::Invalidation, &span)
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Purged(None)), "{outcome:?}");
        assert!(!is_cached(store, "/active").await);
        assert_eq!(store.tags(), 0, "the tag index outlived the entry");
        let again = store
            .purge(PurgeTarget::Active(&active), PurgeType::Invalidation, &span)
            .await
            .unwrap();
        assert!(matches!(again, PurgeOutcome::NotFound), "{again:?}");

        fill(store, "/exact", &[], 16).await;
        let exact = CacheEntryKey::key_only(CacheKey::new("/exact", "/exact").to_compact());
        let outcome = store
            .purge(PurgeTarget::Exact(&exact), PurgeType::Eviction, &span)
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Purged(None)), "{outcome:?}");
        assert!(!is_cached(store, "/exact").await);
    }

    /// A fill still being written when Pingora purges it must not be admitted
    /// afterwards: the hook reports it as purged and the finish is dropped.
    #[tokio::test]
    async fn pingoras_purge_hook_invalidates_a_fill_in_progress() {
        let store = store_of(1 << 20);
        let span = Span::inactive().handle();
        let now = SystemTime::now();
        let meta = CacheMeta::new(
            now + Duration::from_secs(600),
            now,
            0,
            0,
            ResponseHeader::build(200, None).unwrap(),
        );
        let key = CacheKey::new("/filling", "/filling");
        let mut writer = store.get_miss_handler(&key, &meta, &span).await.unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();

        let outcome = store
            .purge(
                PurgeTarget::Active(&key.to_compact()),
                PurgeType::Invalidation,
                &span,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Purged(None)), "{outcome:?}");

        writer
            .write_body(Bytes::from_static(b" rest"), true)
            .await
            .unwrap();
        let _ = writer.finish().await;
        assert!(
            !is_cached(store, "/filling").await,
            "a purged fill was admitted when it finished"
        );
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
                    &CacheKey::new("/p/1", ""),
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
        let key = CacheKey::new("/in-progress", "/in-progress");
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
        let key = CacheKey::new("/old-build", "/old-build");
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
    async fn a_transient_entry_is_never_persistently_indexed() {
        let store = store_of(1 << 20);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
        header.insert_header("x-tanod-cache-tags", "ghost").unwrap();
        let now = SystemTime::now();
        let meta = CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header);
        let key = CacheKey::new("/transient", "");
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
        let key = CacheKey::new("/streaming", "");

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
        let key = CacheKey::new("/revalidating", "");

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
        let key = CacheKey::new("/absent", "");
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
        let key = CacheKey::new("/abandoned", "");
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
        let key = CacheKey::new("/tag", "");
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
        let key = CacheKey::new("/transient", "");
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

    /// In-progress fills count toward the budget. The fill that cannot fit
    /// is not stored, and the one after that cannot fit even in the headroom
    /// is refused: the logical total never passes budget plus headroom.
    #[tokio::test]
    async fn in_progress_fills_count_toward_the_global_budget() {
        let store = store_of(4096);
        assert_eq!(store.headroom(), 4096, "headroom is clamped to the budget");
        let first = CacheKey::new("/one", "");
        let second = CacheKey::new("/two", "");
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
        b.write_body(Bytes::from(vec![b'b'; 3000]), false)
            .await
            .unwrap();
        assert!(store.bytes_used() > 4096, "b is running on the headroom");
        assert_eq!(store.memory_pressure().fill_abandoned, 1);
        assert!(
            b.write_body(Bytes::from(vec![b'b'; 3000]), false)
                .await
                .is_err()
        );
        assert!(store.bytes_used() <= 4096 + store.headroom());
        assert_eq!(store.memory_pressure().memory_exhausted, 1);
        drop(a);
        drop(b);
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.bytes_pinned(), 0);
    }
    fn tiny_store(budget: usize) -> &'static BoundedStore {
        BoundedStore::new(&CacheDefaults {
            eviction: Eviction::Tinyufo,
            max_memory: ByteSize(budget as u64),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn tinyufo_pressure_evicts_completed_entries_for_a_large_streaming_fill() {
        let budget = 256 * 1024;
        let store = tiny_store(budget);
        fill(store, "/old", &["old-tag"], 1000).await;
        let key = CacheKey::new("/new", "/new");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        let first = budget - store.bytes_used() - 512;
        writer
            .write_body(Bytes::from(vec![b'x'; first]), false)
            .await
            .unwrap();
        writer
            .write_body(Bytes::from(vec![b'y'; 1024]), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert!(store.bytes_used() <= budget);
        assert!(is_cached(store, "/new").await);
        assert!(!is_cached(store, "/old").await);
        assert_eq!(
            store.tags(),
            0,
            "pressure victims must leave no tag mappings"
        );
        assert!(store.evicted() > 0);
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }

    #[tokio::test]
    async fn tinyufo_can_purge_and_replace_the_same_key_without_stale_queue_records() {
        let store = tiny_store(64 * 1024);
        fill(store, "/keep", &["keep"], 1000).await;
        for revision in 0..500 {
            let tag = format!("revision-{revision}");
            fill(store, "/same", &[&tag], 1000 + revision % 200).await;
            assert_eq!(store.entries(), 2);
            assert!(is_cached(store, "/keep").await);
            assert_eq!(store.tags(), 2);
            if revision % 2 == 0 {
                assert_eq!(store.purge_tags([tag.as_str()]).entries, 1);
                assert!(!is_cached(store, "/same").await);
                fill(store, "/same", &[&tag], 1200).await;
            }
            if revision > 0 {
                let old = format!("revision-{}", revision - 1);
                assert_eq!(store.purge_tags([old.as_str()]).entries, 0);
            }
        }
        assert_eq!(store.purge_paths(["/same"]).entries, 1);
        assert!(is_cached(store, "/keep").await);
        assert_eq!(store.purge_all().entries, 1);
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.tags(), 0);
        // Force pressure after the invalidation/replacement workload so lazy
        // records are exercised by eviction, not only by ordinary lookups.
        for i in 0..100 {
            fill(store, &format!("/pressure/{i}"), &[], 8000).await;
        }
        assert!(store.bytes_used() <= store.limit());
        assert!(store.entries() > 0);
    }

    #[tokio::test]
    async fn tinyufo_metadata_growth_shrink_and_new_tags_remain_accounted() {
        let store = tiny_store(64 * 1024);
        fill(store, "/meta", &["old"], 1000).await;
        let key = CacheKey::new("/meta", "/meta");
        for revision in 0..200 {
            let mut header = ResponseHeader::build(200, None).unwrap();
            let tag = format!("new-{revision}");
            header.insert_header("x-tanod-cache-tags", &tag).unwrap();
            header
                .insert_header("x-padding", "a".repeat(revision % 50 * 20))
                .unwrap();
            let now = SystemTime::now();
            let replacement = CacheMeta::new(now + Duration::from_secs(30), now, 0, 0, header);
            assert!(
                store
                    .update_meta(&key, &replacement, &Span::inactive().handle())
                    .await
                    .unwrap()
            );
            assert!(store.bytes_used() <= store.limit());
            assert_eq!(store.tags(), 1);
            assert_eq!(store.purge_tags(["old"]).entries, 0);
        }
        assert_eq!(store.purge_paths(["/meta"]).entries, 1);
        assert_eq!(store.bytes_used(), 0);
        fill(store, "/meta", &["again"], 2000).await;
        assert!(is_cached(store, "/meta").await);
        assert_eq!(store.purge_tags(["again"]).entries, 1);
        assert_eq!(store.bytes_used(), 0);
    }

    #[tokio::test]
    async fn tinyufo_rejected_fills_keep_a_bounded_follower_handoff() {
        let store = tiny_store(1 << 20);
        // Model policy rounding pressure independently of the streaming
        // budget to deterministically exercise TinyLFU admission rejection.
        *store.tiny.as_ref().unwrap().write() = TinyPolicy::new(1500);
        fill(store, "/hot", &[], 1000).await;
        fill(store, "/warm", &[], 1000).await;
        fill(store, "/hot", &[], 1000).await;
        assert!(is_cached(store, "/hot").await);
        let retained = store.bytes_used();
        let key = CacheKey::new("/cold", "/cold");
        let span = Span::inactive().handle();
        let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        writer
            .write_body(Bytes::from(vec![b'a'; 500]), false)
            .await
            .unwrap();
        let (_, mut follower) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert_eq!(follower.read_body().await.unwrap().unwrap().len(), 500);
        writer
            .write_body(Bytes::from(vec![b'b'; 500]), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert_eq!(follower.read_body().await.unwrap().unwrap().len(), 500);
        assert!(follower.read_body().await.unwrap().is_none());
        assert_eq!(
            store.entries(),
            1,
            "cold admission should have been rejected"
        );
        assert_eq!(store.admission_rejected(), 1);
        assert!(
            is_cached(store, "/cold").await,
            "late followers need the handoff"
        );
        assert!(store.bytes_used() > retained);
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert!(!is_cached(store, "/cold").await);
        assert!(is_cached(store, "/hot").await);
        assert_eq!(store.bytes_used(), retained);
    }

    #[tokio::test]
    async fn tinyufo_abandoned_and_invalidated_fills_release_their_reservations() {
        let store = tiny_store(1 << 20);
        let span = Span::inactive().handle();
        let key = CacheKey::new("/partial", "/partial");
        let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        writer
            .write_body(Bytes::from_static(b"first"), false)
            .await
            .unwrap();
        let (_, mut follower) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert_eq!(
            follower.read_body().await.unwrap().unwrap(),
            Bytes::from_static(b"first")
        );
        drop(writer);
        assert_eq!(store.bytes_used(), 0);
        assert!(follower.read_body().await.is_err());
        let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        writer
            .write_body(Bytes::from_static(b"again"), false)
            .await
            .unwrap();
        let (_, mut follower) = store.lookup(&key, &span).await.unwrap().unwrap();
        store.purge_paths(["/partial"]);
        writer.finish().await.unwrap();
        assert!(store.lookup(&key, &span).await.unwrap().is_none());
        assert_eq!(
            follower.read_body().await.unwrap().unwrap(),
            Bytes::from_static(b"again")
        );
        assert!(follower.read_body().await.unwrap().is_none());
        assert_eq!(store.bytes_used(), 0);
        fill(store, "/partial", &[], 1000).await;
        assert!(is_cached(store, "/partial").await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tinyufo_concurrent_fills_purges_and_reads_preserve_accounting() {
        let store = tiny_store(20 * 1024);
        for i in 0..8 {
            fill(store, &format!("/kept/{i}"), &[], 1000).await;
        }
        let mut tasks = tokio::task::JoinSet::new();
        for task in 0..12 {
            tasks.spawn(async move {
                let span = Span::inactive().handle();
                for turn in 0..40 {
                    let path = format!("/p/{}", (task + turn) % 8);
                    let key = CacheKey::new(path.as_str(), path.as_str());
                    let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
                    writer
                        .write_body(Bytes::from(vec![b'x'; 500]), false)
                        .await
                        .unwrap();
                    tokio::task::yield_now().await;
                    if turn % 3 == 0 {
                        store.purge_paths([path.as_str()]);
                    }
                    writer
                        .write_body(Bytes::from(vec![b'y'; 500]), true)
                        .await
                        .unwrap();
                    writer.finish().await.unwrap();
                    let _ = store.lookup(&key, &span).await.unwrap();
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert!(store.temp.read().is_empty());
        let used = store.bytes_used();
        let cached = store.cached.read();
        assert_eq!(used, cached.values().map(Stored::size).sum::<usize>());
        assert!(
            store.evicted() > 0,
            "concurrent workload must exercise eviction"
        );
        assert!(store.bytes_used() <= store.limit());
        for (tag, keys) in store.tag_index.read().iter() {
            for key in keys {
                assert!(cached.get(key).unwrap().tags.contains(tag));
            }
        }
    }
    #[tokio::test]
    async fn tinyufo_preserves_expiry_and_stale_windows_through_revalidation() {
        let store = tiny_store(1 << 20);
        let key = CacheKey::new("/expired", "/expired");
        let span = Span::inactive().handle();
        let now = SystemTime::now();
        let stale = CacheMeta::new(
            now - Duration::from_secs(1),
            now - Duration::from_secs(10),
            60,
            120,
            ResponseHeader::build(200, None).unwrap(),
        );
        let mut writer = store.get_miss_handler(&key, &stale, &span).await.unwrap();
        writer
            .write_body(Bytes::from_static(b"cached body"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        let (found, _) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert!(!found.is_fresh(now));
        assert!(found.serve_stale_while_revalidate(now));
        assert!(found.serve_stale_if_error(now));
        let fresh = CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            30,
            90,
            ResponseHeader::build(200, None).unwrap(),
        );
        assert!(store.update_meta(&key, &fresh, &span).await.unwrap());
        let (found, mut hit) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert!(found.is_fresh(now));
        assert_eq!(found.stale_while_revalidate_sec(), 30);
        assert_eq!(found.stale_if_error_sec(), 90);
        assert_eq!(
            hit.read_body().await.unwrap().unwrap(),
            Bytes::from_static(b"cached body")
        );
    }

    #[tokio::test]
    async fn tinyufo_purge_hides_a_rejected_handoff_from_new_readers() {
        let store = tiny_store(1 << 20);
        *store.tiny.as_ref().unwrap().write() = TinyPolicy::new(1500);
        fill(store, "/hot", &[], 1000).await;
        fill(store, "/warm", &[], 1000).await;
        fill(store, "/hot", &[], 1000).await;
        fill(store, "/cold", &[], 1000).await;
        let key = CacheKey::new("/cold", "/cold");
        let span = Span::inactive().handle();
        let (_, mut attached) = store.lookup(&key, &span).await.unwrap().unwrap();
        store.purge_paths(["/cold"]);
        assert!(store.lookup(&key, &span).await.unwrap().is_none());
        assert_eq!(attached.read_body().await.unwrap().unwrap().len(), 1000);
        assert!(attached.read_body().await.unwrap().is_none());
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert_eq!(store.entries(), 1);
        assert!(is_cached(store, "/hot").await);
    }
}
