//! Body storage without per-read copies, and completion that does not stop
//! lookups.
//!
//! The contract under test: a body is delivered byte-exact to the leader,
//! every follower and every later hit, whatever the chunking; a finished
//! entry is read by sharing its memory rather than copying it; a fill split
//! into many small writes is stored as few large segments; and a lookup racing
//! a fill's completion finds the fill or the entry, never neither.

use super::tests::{cache_cfg, meta};
use super::*;
use pingora_cache::trace::Span;
use proptest::prelude::*;

const POLICIES: [Eviction; 2] = [Eviction::Clock, Eviction::Tinyufo];

fn store(policy: Eviction, budget: usize) -> &'static BoundedStore {
    BoundedStore::new(&CacheDefaults {
        eviction: policy,
        ..cache_cfg(budget)
    })
}

fn span() -> SpanHandle {
    Span::inactive().handle()
}

/// Deterministic, position-dependent content, so a byte delivered at the
/// wrong offset cannot go unnoticed.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| i.to_le_bytes()[0].wrapping_mul(31).wrapping_add(seed))
        .collect()
}

async fn drain(hit: &mut HitHandler) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        assert!(!chunk.is_empty(), "a reader was handed an empty chunk");
        assert!(chunk.len() <= READ_CHUNK, "a chunk exceeded READ_CHUNK");
        out.extend_from_slice(&chunk);
    }
    out
}

// ------------------------------------------------------------ FillBody unit

fn written(body: &mut FillBody, chunks: &[usize], data: &[u8]) {
    let mut at = 0;
    for &len in chunks {
        let piece = &data[at..at + len];
        let owned = (len >= SEAL_AT).then(|| Bytes::copy_from_slice(piece));
        body.append(piece, owned);
        at += len;
    }
}

fn read_all(body: &FillBody, available: usize) -> Vec<u8> {
    let mut cursor = Cursor::default();
    let mut out = Vec::new();
    while let Some(chunk) = body.read(&mut cursor, available) {
        out.extend_from_slice(&chunk);
    }
    out
}

#[test]
fn small_writes_accumulate_and_seal_at_the_threshold() {
    let data = pattern(SEAL_AT * 3, 1);
    let mut body = FillBody::default();
    // 3 * SEAL_AT bytes in 100-byte writes.
    let chunks = vec![100; data.len() / 100];
    written(&mut body, &chunks, &data[..chunks.len() * 100]);
    assert!(body.open.len() < SEAL_AT, "the tail outgrew SEAL_AT");
    // One segment per SEAL_AT-ish run of small writes, not one per write.
    assert!(!body.sealed.is_empty());
    assert!(
        body.sealed
            .iter()
            .all(|s| (SEAL_AT..SEAL_AT + 100).contains(&s.len())),
        "segments were not sealed at the threshold"
    );
    assert_eq!(read_all(&body, body.len()), &data[..body.len()]);
}

#[test]
fn a_large_write_seals_the_tail_before_it_and_keeps_order() {
    let data = pattern(SEAL_AT * 5, 2);
    let mut body = FillBody::default();
    written(
        &mut body,
        &[10, 20, SEAL_AT * 2, 5, SEAL_AT * 3 - 35],
        &data,
    );
    assert_eq!(body.len(), data.len());
    assert_eq!(read_all(&body, body.len()), data);
}

#[test]
fn a_reader_never_sees_past_what_was_published() {
    let data = pattern(SEAL_AT * 2 + 7, 3);
    let mut body = FillBody::default();
    written(&mut body, &[SEAL_AT * 2 + 7], &data);
    for limit in [0, 1, 7, SEAL_AT, SEAL_AT * 2 + 6] {
        assert_eq!(read_all(&body, limit), &data[..limit], "limit {limit}");
    }
}

#[test]
fn sealing_the_tail_keeps_a_reader_mid_tail_at_its_offset() {
    let data = pattern(SEAL_AT + 50, 4);
    let mut body = FillBody::default();
    let mut cursor = Cursor::default();
    body.append(&data[..30], None);
    assert_eq!(body.read(&mut cursor, 10).unwrap().as_ref(), &data[..10]);
    // The next write seals the tail the reader is part-way through.
    body.append(&data[30..], None);
    let mut rest = Vec::new();
    while let Some(chunk) = body.read(&mut cursor, body.len()) {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(rest, &data[10..]);
}

#[test]
fn freezing_large_segments_shares_them_without_copying() {
    let data = pattern(READ_CHUNK * 3, 5);
    let mut body = FillBody::default();
    written(&mut body, &[READ_CHUNK, READ_CHUNK, READ_CHUNK], &data);
    let pointers: Vec<_> = body.sealed.iter().map(|s| s.as_ptr()).collect();
    let frozen = body.freeze();
    assert_eq!(
        frozen
            .segments
            .iter()
            .map(|s| s.as_ptr())
            .collect::<Vec<_>>(),
        pointers,
        "large segments were copied on freeze"
    );
    assert_eq!(frozen.len, data.len());
}

#[test]
fn freezing_many_small_segments_compacts_them() {
    let data = pattern(READ_CHUNK * 2 + 123, 6);
    let mut body = FillBody::default();
    let mut chunks = vec![SEAL_AT; data.len() / SEAL_AT];
    chunks.push(data.len() % SEAL_AT);
    written(&mut body, &chunks, &data);
    assert!(body.sealed.len() > 4);
    let frozen = body.freeze();
    assert_eq!(frozen.segments.len(), data.len().div_ceil(READ_CHUNK));
    assert!(frozen.segments.iter().all(|s| s.len() <= READ_CHUNK));
    let joined: Vec<u8> = frozen.segments.iter().flat_map(|s| s.to_vec()).collect();
    assert_eq!(joined, data);
}

#[test]
fn two_small_segments_are_joined_so_a_hit_is_one_write() {
    let data = pattern(16 * 1024, 11);
    let mut body = FillBody::default();
    written(&mut body, &[6 * 1024, 10 * 1024], &data);
    assert_eq!(body.sealed.len(), 2);
    let frozen = body.freeze();
    assert_eq!(frozen.segments.len(), 1);
    assert_eq!(frozen.segments[0].as_ref(), &data[..]);
}

#[test]
fn freezing_an_empty_body_yields_an_empty_entry() {
    let frozen = FillBody::default().freeze();
    assert_eq!(frozen.len, 0);
    assert!(frozen.segments.is_empty());
}

proptest! {
    /// Any chunking, any read limits: the bytes read back are the bytes
    /// written, and freezing preserves them exactly.
    #[test]
    fn any_chunking_round_trips(
        sizes in proptest::collection::vec(1usize..(3 * SEAL_AT), 1..40),
        seed in any::<u8>(),
    ) {
        let total: usize = sizes.iter().sum();
        let data = pattern(total, seed);
        let mut body = FillBody::default();
        let mut cursor = Cursor::default();
        let mut followed = Vec::new();
        let mut at = 0;
        for len in &sizes {
            let piece = &data[at..at + len];
            let owned = (*len >= SEAL_AT).then(|| Bytes::copy_from_slice(piece));
            body.append(piece, owned);
            at += len;
            // A follower that reads as far as each write lets it.
            while let Some(chunk) = body.read(&mut cursor, at) {
                followed.extend_from_slice(&chunk);
            }
        }
        prop_assert_eq!(&followed, &data);
        let frozen = body.freeze();
        prop_assert_eq!(frozen.len, total);
        let joined: Vec<u8> = frozen.segments.iter().flat_map(|s| s.to_vec()).collect();
        prop_assert_eq!(joined, data);
        // A hit takes exactly as many reads as a contiguous body would.
        let reads: usize = frozen.segments.iter().map(|s| s.len().div_ceil(READ_CHUNK)).sum();
        prop_assert_eq!(reads, total.div_ceil(READ_CHUNK));
    }
}

// ------------------------------------------------------------ through Storage

/// Leader and followers attach before the body exists; the body then arrives
/// in a mix of tiny and large writes. Everyone gets exactly the body, and so
/// does every hit afterwards.
#[tokio::test]
async fn followers_and_later_hits_receive_the_exact_body() {
    for policy in POLICIES {
        let store = store(policy, 32 << 20);
        let key = CacheKey::new("/exact", "/exact");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &span())
            .await
            .unwrap();
        let tag = writer.streaming_write_tag().unwrap().to_vec();
        let (_, mut leader) = store
            .lookup_streaming_write(&key, Some(&tag), &span())
            .await
            .unwrap()
            .unwrap();
        let mut followers = Vec::new();
        for _ in 0..4 {
            followers.push(store.lookup(&key, &span()).await.unwrap().unwrap().1);
        }
        let data = pattern(READ_CHUNK * 2 + 4321, 9);
        let sizes = [1, 2, 300, SEAL_AT, 17, READ_CHUNK + 5, 9000];
        let mut at = 0;
        let mut i = 0;
        while at < data.len() {
            let len = sizes[i % sizes.len()].min(data.len() - at);
            writer
                .write_body(Bytes::copy_from_slice(&data[at..at + len]), false)
                .await
                .unwrap();
            at += len;
            i += 1;
        }
        writer.finish().await.unwrap();
        assert_eq!(drain(&mut leader).await, data, "{policy:?} leader");
        for follower in &mut followers {
            assert_eq!(drain(follower).await, data, "{policy:?} follower");
        }
        let (_, mut hit) = store.lookup(&key, &span()).await.unwrap().unwrap();
        assert_eq!(drain(&mut hit).await, data, "{policy:?} hit");
    }
}

/// The claim behind "zero-copy": two hits on one entry are handed slices of
/// the same memory, not two copies of it.
#[tokio::test]
async fn hits_share_the_stored_body_instead_of_copying_it() {
    let store = store(Eviction::Clock, 32 << 20);
    let key = CacheKey::new("/shared", "/shared");
    let mut writer = store
        .get_miss_handler(&key, &meta(), &span())
        .await
        .unwrap();
    let data = pattern(READ_CHUNK + 100, 7);
    writer
        .write_body(Bytes::copy_from_slice(&data), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();

    let mut pointers = Vec::new();
    for _ in 0..2 {
        let (_, mut hit) = store.lookup(&key, &span()).await.unwrap().unwrap();
        let first = hit.read_body().await.unwrap().unwrap();
        pointers.push(first.as_ptr());
        assert_eq!(first.as_ref(), &data[..READ_CHUNK]);
    }
    assert_eq!(pointers[0], pointers[1], "each hit copied the body");
}

#[tokio::test]
async fn seeking_a_finished_entry_reads_from_any_offset() {
    let store = store(Eviction::Clock, 32 << 20);
    let key = CacheKey::new("/seek", "/seek");
    let mut writer = store
        .get_miss_handler(&key, &meta(), &span())
        .await
        .unwrap();
    let data = pattern(READ_CHUNK * 2 + 10, 8);
    for piece in data.chunks(SEAL_AT + 1) {
        writer
            .write_body(Bytes::copy_from_slice(piece), false)
            .await
            .unwrap();
    }
    writer.finish().await.unwrap();
    for start in [
        0,
        1,
        SEAL_AT,
        READ_CHUNK - 1,
        READ_CHUNK,
        data.len() - 1,
        data.len(),
    ] {
        let (_, mut hit) = store.lookup(&key, &span()).await.unwrap().unwrap();
        assert!(hit.can_seek());
        hit.seek(start, None).unwrap();
        assert_eq!(drain(&mut hit).await, &data[start..], "seek to {start}");
    }
}

/// A lookup racing an ordinary (stored, not transient) completion must find
/// the fill or the stored entry every time. Completion no longer excludes
/// lookups, so this is the property that replaced the exclusion.
#[tokio::test(start_paused = true)]
async fn lookup_during_stored_completion_cannot_observe_a_gap() {
    for policy in POLICIES {
        for _ in 0..16 {
            let store = store(policy, 1 << 20);
            let key = CacheKey::new("stored-race", "/page");
            let mut writer = store
                .get_miss_handler(&key, &meta(), &span())
                .await
                .unwrap();
            writer
                .write_body(Bytes::from_static(b"complete"), true)
                .await
                .unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let b = barrier.clone();
            let runtime = tokio::runtime::Handle::current();
            let thread = std::thread::spawn(move || {
                b.wait();
                runtime.block_on(writer.finish()).unwrap();
            });
            barrier.wait();
            for _ in 0..500 {
                let (_, mut hit) = store
                    .lookup(&key, &span())
                    .await
                    .unwrap()
                    .expect("completion exposed a gap between temp and cached");
                assert_eq!(drain(&mut hit).await, b"complete");
            }
            thread.join().unwrap();
            assert!(store.temp.read().is_empty());
            assert_eq!(store.entries(), 1);
        }
    }
}

/// Purges racing completions on real threads: whatever the interleaving, an
/// entry survives only if its fill completed before the purge, nothing is
/// left half-admitted, and the accounting balances.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completions_racing_purges_leave_consistent_accounting() {
    for policy in POLICIES {
        let store = store(policy, 4 << 20);
        let mut tasks = tokio::task::JoinSet::new();
        for worker in 0..8u32 {
            tasks.spawn(async move {
                for round in 0..50u32 {
                    let path = format!("/race/{}", (worker * 7 + round) % 13);
                    let key = CacheKey::new(path.clone(), path);
                    let span = span();
                    if round % 5 == worker % 5 {
                        store.purge_all();
                        continue;
                    }
                    let Ok(mut writer) = store.get_miss_handler(&key, &meta(), &span).await else {
                        continue;
                    };
                    let data = pattern(SEAL_AT * 2 + round as usize, round.to_le_bytes()[0]);
                    for piece in data.chunks(1500) {
                        writer
                            .write_body(Bytes::copy_from_slice(piece), false)
                            .await
                            .unwrap();
                        tokio::task::yield_now().await;
                    }
                    writer.finish().await.unwrap();
                    if let Some((_, mut hit)) = store.lookup(&key, &span).await.unwrap() {
                        let read = drain(&mut hit).await;
                        // Another worker may have replaced the entry with a
                        // body for a different round; any body must be whole.
                        assert!(read.len() >= SEAL_AT * 2, "a truncated body was served");
                    }
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert!(
            store.temp.read().is_empty(),
            "{policy:?}: a fill was left behind"
        );
        assert_eq!(store.bytes_pinned(), 0, "{policy:?}: pinned bytes leaked");
        let cached = store.cached.read();
        assert_eq!(
            store.bytes_used(),
            cached.values().map(Stored::size).sum::<usize>(),
            "{policy:?}: accounting drifted"
        );
    }
}

/// Reservations that fit take the fast path, ones that need eviction the slow
/// one; interleaved across threads, the budget still holds at every instant
/// and every byte comes back.
#[test]
fn concurrent_fast_and_slow_reservations_never_overcommit() {
    let store = store(Eviction::Clock, 256 * 1024);
    let threads: Vec<_> = (0..6usize)
        .map(|t| {
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for i in 0..2_000usize {
                    let amount = 1 + (t * 7919 + i * 104_729) % 9_000;
                    match store.reserve_fill(amount, true, false) {
                        Grant::Budget => held.push(amount),
                        Grant::Refused => {}
                        Grant::Headroom => panic!("headroom granted without being offered"),
                    }
                    assert!(store.bytes_used() <= store.limit(), "overcommitted");
                    if held.len() > 3 {
                        store.release(held.remove(0));
                    }
                }
                for amount in held {
                    store.release(amount);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(store.bytes_used(), 0);
    assert_eq!(store.bytes_pinned(), 0);
}

// ------------------------------------------------------------ eviction queue

async fn put(store: &'static BoundedStore, path: &str, len: usize) {
    let key = CacheKey::new(path, path);
    let mut writer = store
        .get_miss_handler(&key, &meta(), &span())
        .await
        .unwrap();
    writer
        .write_body(Bytes::from(vec![b'q'; len]), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();
}

async fn present(store: &'static BoundedStore, path: &str) -> bool {
    store
        .lookup(&CacheKey::new(path, path), &span())
        .await
        .unwrap()
        .is_some()
}

/// A replaced entry goes to the back of the queue, exactly as when its old
/// record was removed eagerly: the next victim is the oldest entry that was
/// not replaced, and the replacement is not evicted through its stale record.
#[tokio::test]
async fn a_replacement_is_not_evicted_through_its_old_queue_record() {
    // One entry's accounted size, measured rather than assumed.
    let probe = store(Eviction::Clock, 1 << 20);
    put(probe, "/a", 1024).await;
    let entry = probe.bytes_used();
    for policy in [Eviction::Clock, Eviction::Fifo] {
        // Room for four entries (three stored plus a replacement in flight)
        // but not for a fifth half again as large.
        let store = store(policy, 4 * entry + entry / 4);
        for path in ["/a", "/b", "/c"] {
            put(store, path, 1024).await;
        }
        // Replace /a: its first record is now stale and sits at the front.
        put(store, "/a", 1024).await;
        assert_eq!(
            store.evicted(),
            0,
            "{policy:?}: the replacement needed room"
        );
        assert_eq!(store.order.read().records.len(), 4);
        // A larger entry forces exactly one eviction.
        put(store, "/d", 1024 + entry / 2).await;
        assert_eq!(store.evicted(), 1, "{policy:?}");
        assert!(
            present(store, "/a").await,
            "{policy:?}: the replacement was evicted"
        );
        assert!(
            !present(store, "/b").await,
            "{policy:?}: the oldest entry survived"
        );
        assert!(present(store, "/c").await, "{policy:?}");
        assert!(present(store, "/d").await, "{policy:?}");
    }
}

/// Replacing and purging without ever evicting cannot grow the queue past a
/// bounded multiple of the entries it describes.
#[tokio::test]
async fn retired_queue_records_stay_bounded() {
    let store = store(Eviction::Clock, 64 << 20);
    for round in 0..2_000 {
        put(store, &format!("/k{}", round % 10), 64).await;
        if round % 97 == 0 {
            store.purge_paths(["/k3"]);
        }
    }
    let live = store.entries();
    assert!(live <= 10);
    assert!(
        store.order.read().records.len() <= live * 2 + 64,
        "{} records for {live} entries",
        store.order.read().records.len()
    );
    assert_eq!(store.evicted(), 0);
}

/// The same property under real contention: lookup threads spinning on one
/// key while its fill completes, thousands of times. A lookup that misses
/// both maps during a completion is the bug this guards against; it is a
/// window of a few instructions, so it takes many completions to hit.
#[test]
fn spinning_lookups_never_miss_a_completing_fill() {
    use std::sync::atomic::AtomicBool;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for policy in POLICIES {
        let missed = Arc::new(AtomicU64::new(0));
        // Fewer under a debug build, where each lookup is slow enough that
        // the window is wider anyway.
        let rounds = if cfg!(debug_assertions) { 500 } else { 2_000 };
        for _ in 0..rounds {
            let store = store(policy, 1 << 20);
            let key = CacheKey::new("spin", "/spin");
            let mut writer = runtime
                .block_on(store.get_miss_handler(&key, &meta(), &span()))
                .unwrap();
            runtime
                .block_on(writer.write_body(Bytes::from_static(b"done"), true))
                .unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let start = Arc::new(std::sync::Barrier::new(5));
            let spinners: Vec<_> = (0..4)
                .map(|_| {
                    let (done, start, missed) = (done.clone(), start.clone(), missed.clone());
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .build()
                            .unwrap();
                        let key = CacheKey::new("spin", "/spin");
                        start.wait();
                        // Keep looking until a little after completion.
                        let mut after = 0;
                        while after < 50 {
                            if done.load(Ordering::Acquire) {
                                after += 1;
                            }
                            if rt.block_on(store.lookup(&key, &span())).unwrap().is_none() {
                                missed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();
            start.wait();
            runtime.block_on(writer.finish()).unwrap();
            done.store(true, Ordering::Release);
            for s in spinners {
                s.join().unwrap();
            }
        }
        assert_eq!(
            missed.load(Ordering::Relaxed),
            0,
            "{policy:?}: lookups missed a completing fill"
        );
    }
}
