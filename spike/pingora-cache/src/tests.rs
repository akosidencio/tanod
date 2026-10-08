//! The three questions the spike exists to answer.

use super::*;
use pingora_cache::CacheMeta;
use pingora_cache::trace::Span;
use pingora_http::ResponseHeader;
use std::time::{Duration, SystemTime};

fn key(path: &str) -> CacheKey {
    CacheKey::new(path, "")
}

/// `fresh_until` in the future: an ordinary cacheable response.
fn meta_fresh(secs: u64) -> CacheMeta {
    let created = SystemTime::now();
    CacheMeta::new(
        created + Duration::from_secs(secs),
        created,
        0,
        0,
        ResponseHeader::build(200, None).unwrap(),
    )
}

/// `fresh_until == created`: born stale. Collapses an in-flight herd through
/// the cache lock, then is never served as fresh to anyone afterwards.
fn meta_born_stale() -> CacheMeta {
    let created = SystemTime::now();
    CacheMeta::new(
        created,
        created,
        0,
        0,
        ResponseHeader::build(200, None).unwrap(),
    )
}

fn span() -> Span {
    Span::inactive()
}

#[tokio::test]
async fn question_1_we_can_own_the_store_and_bound_it() {
    // MemCache is an unbounded HashMap. This is the same trait, with a budget.
    let store = BoundedStore::new(1024);

    for i in 0..50 {
        let k = key(&format!("/page/{i}"));
        let mut miss = store
            .get_miss_handler(&k, &meta_fresh(60), &span().handle())
            .await
            .unwrap();
        miss.write_body(Bytes::from(vec![b'x'; 100]), true)
            .await
            .unwrap();
        miss.finish().await.unwrap();
    }

    assert!(
        store.bytes_used() <= 1024,
        "budget exceeded: {} bytes over a 1024 byte ceiling",
        store.bytes_used()
    );
    assert!(store.entries() > 0, "eviction removed everything");
    assert!(
        store.entries() < 50,
        "nothing was evicted, so the budget was never enforced"
    );
}

#[tokio::test]
async fn question_1b_an_oversized_body_is_not_stored() {
    let store = BoundedStore::new(1024);
    let k = key("/huge");
    let mut miss = store
        .get_miss_handler(&k, &meta_fresh(60), &span().handle())
        .await
        .unwrap();
    miss.write_body(Bytes::from(vec![b'x'; 4096]), true)
        .await
        .unwrap();
    miss.finish().await.unwrap();

    assert_eq!(
        store.entries(),
        0,
        "an entry larger than the whole budget was admitted"
    );
    assert!(store.lookup(&k, &span().handle()).await.unwrap().is_none());
}

#[tokio::test]
async fn question_2_a_waiter_gets_the_first_chunk_before_the_render_finishes() {
    // The question that decides whether coalescing destroys streaming.
    // A leader that streams a shell at t=0 and finishes at t=300ms must not
    // make every waiter wait 300ms for its first byte.
    let store = BoundedStore::new(1 << 20);
    let k = key("/streamed");

    let mut leader = store
        .get_miss_handler(&k, &meta_fresh(60), &span().handle())
        .await
        .unwrap();
    let tag = leader
        .streaming_write_tag()
        .map(|t| t.to_vec())
        .expect("streaming write tag");

    // The leader emits the shell immediately.
    leader
        .write_body(Bytes::from_static(b"<html><body>shell"), false)
        .await
        .unwrap();

    // A waiter attaches to the write in progress.
    let (_meta, mut waiter) = store
        .lookup_streaming_write(&k, Some(&tag), &span().handle())
        .await
        .unwrap()
        .expect("waiter should attach to the in-flight write");

    let first = tokio::time::timeout(Duration::from_millis(50), waiter.read_body())
        .await
        .expect("waiter blocked instead of receiving the shell")
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"<html><body>shell");

    // Only now does the leader finish the render.
    tokio::time::sleep(Duration::from_millis(30)).await;
    leader
        .write_body(Bytes::from_static(b"</body></html>"), true)
        .await
        .unwrap();
    leader.finish().await.unwrap();

    let rest = waiter.read_body().await.unwrap().unwrap();
    assert_eq!(&rest[..], b"</body></html>");
    assert!(
        waiter.read_body().await.unwrap().is_none(),
        "stream should end"
    );
}

#[tokio::test]
async fn question_3_collapse_with_nothing_retained() {
    // Spec §94: 500 simultaneous requests, one render, nothing persisted.
    // pingora-cache cannot share a response it considers uncacheable — its
    // lock releases readers to fetch independently instead. But an entry born
    // stale collapses the in-flight herd and is never served as fresh after.
    let store = BoundedStore::new(1 << 20);
    let k = key("/ttl-zero");

    let mut leader = store
        .get_miss_handler(&k, &meta_born_stale(), &span().handle())
        .await
        .unwrap();
    let tag = leader.streaming_write_tag().map(|t| t.to_vec()).unwrap();
    leader
        .write_body(Bytes::from_static(b"rendered once"), false)
        .await
        .unwrap();

    // Everyone in the herd reads the single render.
    let mut waiters = Vec::new();
    for _ in 0..20 {
        let (_m, h) = store
            .lookup_streaming_write(&k, Some(&tag), &span().handle())
            .await
            .unwrap()
            .expect("waiter attaches");
        waiters.push(h);
    }
    for w in waiters.iter_mut() {
        let body = w.read_body().await.unwrap().unwrap();
        assert_eq!(&body[..], b"rendered once");
    }

    leader.write_body(Bytes::new(), true).await.unwrap();
    leader.finish().await.unwrap();

    // It landed in the store, but stale on arrival, so no later request is
    // served from it as fresh.
    let (meta, _h) = store
        .lookup(&k, &span().handle())
        .await
        .unwrap()
        .expect("entry exists");
    assert!(
        !meta.is_fresh(SystemTime::now()),
        "entry should be born stale"
    );
}

#[tokio::test]
async fn a_streaming_lookup_for_a_vanished_write_does_not_panic() {
    // MemCache does `.expect("must have partial write in progress")` here.
    // Under a race between the lock release and the lookup, that is a crash.
    let store = BoundedStore::new(1 << 20);
    let k = key("/raced");

    let mut leader = store
        .get_miss_handler(&k, &meta_fresh(60), &span().handle())
        .await
        .unwrap();
    let tag = leader.streaming_write_tag().map(|t| t.to_vec()).unwrap();
    leader
        .write_body(Bytes::from_static(b"done"), true)
        .await
        .unwrap();
    leader.finish().await.unwrap(); // the temp object is gone now

    // A late waiter arrives with a tag that no longer names a live write.
    let result = store
        .lookup_streaming_write(&k, Some(&tag), &span().handle())
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "a vanished tag must not select a different write"
    );
}

#[tokio::test]
async fn a_malformed_write_tag_is_a_miss_not_a_crash() {
    let store = BoundedStore::new(1 << 20);
    let k = key("/bad-tag");
    let out = store
        .lookup_streaming_write(&k, Some(b"nope"), &span().handle())
        .await
        .unwrap();
    assert!(out.is_none());
}
