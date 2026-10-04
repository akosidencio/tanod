//! Spike: can Tanod sit on `pingora-cache` instead of building its own store
//! and coalescer?
//!
//! Three questions, each answered by a test below:
//!
//! 1. Can we own the store? `MemCache` is the only `Storage` implementation
//!    and is documented "for testing only" — it is an unbounded `HashMap`, and
//!    its `lookup_streaming_write` is `.expect("must have partial write in
//!    progress")`. Both are fixable in our own implementation.
//! 2. Can a waiter follow the leader's *stream*, rather than waiting for the
//!    leader to finish? This decides whether coalescing destroys streaming.
//! 3. Can we express collapse-with-nothing-retained — one render, N responses,
//!    nothing left behind?

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use pingora_cache::CacheMeta;
use pingora_cache::key::{CacheHashKey, CacheKey, CompactCacheKey};
use pingora_cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
    streaming_write::U64WriteId,
};
use pingora_cache::trace::SpanHandle;
use pingora_error::{Error, ErrorType, Result};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::watch;

type BinaryMeta = (Vec<u8>, Vec<u8>);

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
#[derive(Clone)]
struct Stored {
    meta: BinaryMeta,
    body: Arc<Vec<u8>>,
}

/// A write in progress. Readers attach to this and follow it.
struct Temp {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
}

impl Temp {
    fn new(meta: BinaryMeta) -> Self {
        let (tx, _rx) = watch::channel(Written::Partial(0));
        Temp {
            meta,
            body: Arc::new(RwLock::new(Vec::new())),
            written: Arc::new(tx),
        }
    }
}

/// An in-memory store with a byte budget.
///
/// The budget is the whole point: `MemCache` is an unbounded map, which is the
/// substantive reason it is not for production, and a cache that cannot be
/// bounded is a memory-exhaustion vector in a component whose job is to absorb
/// traffic spikes.
pub struct BoundedStore {
    cached: RwLock<HashMap<String, Stored>>,
    /// Insertion order, for eviction. A real implementation would use
    /// TinyUFO; FIFO is enough to prove the budget is enforceable.
    order: RwLock<VecDeque<String>>,
    temp: RwLock<HashMap<String, HashMap<u64, Temp>>>,
    used: AtomicUsize,
    max_bytes: usize,
    next_id: AtomicU64,
}

impl BoundedStore {
    pub fn new(max_bytes: usize) -> &'static Self {
        // `Storage` takes `&'static self` throughout (upstream marks this a
        // TODO). Leaking once at startup is the intended shape.
        Box::leak(Box::new(BoundedStore {
            cached: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            temp: RwLock::new(HashMap::new()),
            used: AtomicUsize::new(0),
            max_bytes,
            next_id: AtomicU64::new(0),
        }))
    }

    pub fn bytes_used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn entries(&self) -> usize {
        self.cached.read().len()
    }

    fn admit(&self, key: String, obj: Stored) {
        let size = obj.body.len() + obj.meta.0.len() + obj.meta.1.len();
        if size > self.max_bytes {
            return; // oversized entries are streamed to the client, never stored
        }
        {
            let mut cached = self.cached.write();
            let mut order = self.order.write();
            while self.used.load(Ordering::Relaxed) + size > self.max_bytes {
                let Some(victim) = order.pop_front() else {
                    break;
                };
                if let Some(v) = cached.remove(&victim) {
                    let freed = v.body.len() + v.meta.0.len() + v.meta.1.len();
                    self.used.fetch_sub(freed, Ordering::Relaxed);
                }
            }
            if let Some(old) = cached.insert(key.clone(), obj) {
                let freed = old.body.len() + old.meta.0.len() + old.meta.1.len();
                self.used.fetch_sub(freed, Ordering::Relaxed);
                order.retain(|k| k != &key);
            }
            order.push_back(key);
        }
        self.used.fetch_add(size, Ordering::Relaxed);
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
        let Some(obj) = self.cached.read().get(&hash).cloned() else {
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&obj.meta.0, &obj.meta.1)?;
        Ok(Some((
            meta,
            Box::new(CompleteHit {
                body: obj.body,
                read: 0,
                done: false,
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
            // A tagged lookup must never fall back to another write for this
            // key; Pingora requires the returned body to match the tag.
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
        let hash = key.combined();
        let temp = Temp::new(meta.serialize()?);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handler = FollowableMiss {
            store: self,
            key: hash.clone(),
            id: id.into(),
            body: temp.body.clone(),
            written: temp.written.clone(),
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
        let hash = key.combined();
        let removed = self.cached.write().remove(&hash);
        if let Some(v) = &removed {
            self.used.fetch_sub(v.body.len(), Ordering::Relaxed);
            self.order.write().retain(|k| k != &hash);
        }
        Ok(removed.is_some())
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<bool> {
        let hash = key.combined();
        let mut cached = self.cached.write();
        let Some(obj) = cached.get_mut(&hash) else {
            return Ok(false);
        };
        obj.meta = meta.serialize()?;
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
    body: Arc<Vec<u8>>,
    read: usize,
    done: bool,
}

#[async_trait]
impl HandleHit for CompleteHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Bytes::copy_from_slice(&self.body[self.read..])))
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
        self.read = start.min(self.body.len());
        self.done = false;
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
                let chunk = Bytes::copy_from_slice(&self.body.read()[self.read..available]);
                self.read = available;
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
}

#[async_trait]
impl HandleMiss for FollowableMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
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

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let id: u64 = self.id.into();
        let temp = self
            .store
            .temp
            .write()
            .get_mut(&self.key)
            .and_then(|m| m.remove(&id));
        let Some(temp) = temp else {
            return Err(Error::explain(ErrorType::InternalError, "write vanished"));
        };
        // Make sure any follower that has not drained yet sees the end.
        let total = self.written.borrow().bytes();
        self.written.send_replace(Written::Complete(total));

        let body = Arc::new(temp.body.read().clone());
        let size = body.len();
        self.store.admit(
            self.key.clone(),
            Stored {
                meta: temp.meta,
                body,
            },
        );
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.id.as_bytes())
    }
}

#[cfg(test)]
mod tests;
