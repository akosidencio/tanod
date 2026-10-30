//! The transient handoff window (cache review finding C11).
//!
//! A `TransientOnly` fill stays in `temp` for [`TRANSIENT_HANDOFF`] after it
//! completes, because Pingora wakes its cache-lock waiters into an ordinary
//! lookup and removing the fill at once would send them all to the origin.
//! These schedules pin what that window can and cannot do: it is bounded from
//! completion, attaching does not extend it, purge and namespace retirement
//! close it to new readers while existing readers finish, eviction neither
//! admits nor reclaims it, and every reservation comes back.
use super::phase3_tests::{drain, span, store};
use super::tests::meta;
use super::*;
use std::time::Duration;

const POLICIES: [Eviction; 2] = [Eviction::Clock, Eviction::Tinyufo];

fn transient_meta() -> CacheMeta {
    let mut meta = meta();
    meta.response_header_mut()
        .insert_header(crate::cache::TRANSIENT_HEADER, "1")
        .unwrap();
    meta.response_header_mut()
        .insert_header("x-tanod-cache-tags", "product")
        .unwrap();
    meta
}

/// Complete a transient fill, leaving it in its handoff.
async fn handoff(store: &'static BoundedStore, key: &CacheKey, body: &'static [u8]) {
    let mut writer = store
        .get_miss_handler(key, &transient_meta(), &span())
        .await
        .unwrap();
    writer
        .write_body(Bytes::from_static(body), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();
}

async fn stored(store: &'static BoundedStore, path: &str, len: usize) {
    let key = CacheKey::new(path, path);
    let mut writer = store
        .get_miss_handler(&key, &meta(), &span())
        .await
        .unwrap();
    writer
        .write_body(Bytes::from(vec![b's'; len]), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();
}

async fn attachable(store: &'static BoundedStore, key: &CacheKey) -> bool {
    store.lookup(key, &span()).await.unwrap().is_some()
}

#[tokio::test(start_paused = true)]
async fn the_handoff_is_bounded_from_completion_and_attaching_does_not_extend_it() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        let key = CacheKey::new("bounded", "/page");
        handoff(store, &key, b"one flight").await;
        assert_eq!(store.entries(), 0, "a transient fill was admitted");
        tokio::time::sleep(TRANSIENT_HANDOFF - Duration::from_millis(5)).await;
        // Attach late in the window and hold the reader without reading.
        let (_, mut late) = store.lookup(&key, &span()).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !attachable(store, &key).await,
            "the handoff outlived its window"
        );
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.bytes_pinned(), 0);
        assert!(store.temp.read().is_empty());
        // The reader already attached keeps its body: it shares the frozen
        // segments by reference, outside the store's accounting.
        assert_eq!(drain(&mut late).await, b"one flight");
    }
}

#[tokio::test(start_paused = true)]
async fn purge_during_the_handoff_closes_it_to_new_readers_only() {
    for policy in POLICIES {
        for scope in 0..4 {
            let store = store(policy, 1 << 20);
            let key = CacheKey::new("purged", "/page");
            handoff(store, &key, b"before purge").await;
            let (_, mut existing) = store.lookup(&key, &span()).await.unwrap().unwrap();
            match scope {
                0 => {
                    store.purge_tags(["product"]);
                }
                1 => {
                    store.purge_paths(["/page"]);
                }
                2 => {
                    store.purge_all();
                }
                _ => {
                    store
                        .purge(
                            PurgeTarget::Active(&key.to_compact()),
                            PurgeType::Invalidation,
                            &span(),
                        )
                        .await
                        .unwrap();
                }
            }
            assert!(
                !attachable(store, &key).await,
                "scope {scope}: purge left it attachable"
            );
            assert_eq!(drain(&mut existing).await, b"before purge");
            // A new flight after the purge registers and hands off on its own;
            // the purged one's timer must not remove it.
            handoff(store, &key, b"after purge").await;
            let (_, mut fresh) = store.lookup(&key, &span()).await.unwrap().unwrap();
            assert_eq!(drain(&mut fresh).await, b"after purge");
            tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
            assert!(!attachable(store, &key).await);
            assert_eq!(store.bytes_used(), 0, "scope {scope}");
            assert!(store.temp.read().is_empty());
            assert_eq!(store.entries(), 0);
            assert_eq!(store.tags(), 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn retirement_during_the_handoff_survives_reusing_the_namespace_id() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        store.activate_namespace(1);
        let key = CacheNamespace(1).key("retired".into(), "/page");
        handoff(store, &key, b"old namespace").await;
        let (_, mut existing) = store.lookup(&key, &span()).await.unwrap().unwrap();
        store.activate_namespace(2);
        assert!(!attachable(store, &key).await);
        // The store keeps no namespace history, so an id can become active
        // again. The fill it retired stays invalidated.
        store.activate_namespace(1);
        assert!(
            !attachable(store, &key).await,
            "a retired handoff came back"
        );
        assert_eq!(drain(&mut existing).await, b"old namespace");
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert_eq!(store.bytes_used(), 0);
        assert!(store.temp.read().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn eviction_during_the_handoff_neither_admits_nor_reclaims_it() {
    for policy in POLICIES {
        // Room for a few stored bodies; the stream below needs eviction.
        let store = store(policy, 16 * 1024);
        let key = CacheKey::new("handoff", "/handoff");
        handoff(store, &key, &[b'h'; 3000]).await;
        let reserved = store.bytes_used();
        assert!(reserved >= 3000);
        for n in 0..12 {
            stored(store, &format!("/stored/{n}"), 3000).await;
        }
        assert!(
            store.evicted.load(Ordering::Relaxed) > 0,
            "no eviction happened"
        );
        assert!(store.bytes_used() <= store.limit());
        assert!(
            attachable(store, &key).await,
            "eviction reclaimed a live handoff"
        );
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert!(!attachable(store, &key).await);
        assert!(store.temp.read().is_empty());
        // Only stored entries remain, and they account for every byte.
        let entries = store.entries();
        assert!(entries > 0);
        store.purge_all();
        assert_eq!(store.bytes_used(), 0, "a handoff reservation leaked");
    }
}

#[tokio::test(start_paused = true)]
async fn repeated_handoffs_under_contention_return_every_reservation() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        let mut tasks = Vec::new();
        for n in 0..400u32 {
            let key = CacheKey::new(format!("contended-{}", n % 8), "/page");
            tasks.push(tokio::spawn(async move {
                if n % 3 == 0 {
                    // Readers racing the writers on the same keys.
                    if let Some((_, mut hit)) = store.lookup(&key, &span()).await.unwrap() {
                        assert_eq!(drain(&mut hit).await, b"contended");
                    }
                } else {
                    handoff(store, &key, b"contended").await;
                }
                if n % 50 == 0 {
                    store.purge_paths(["/page"]);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(
            store.bytes_used() <= 400 * 1024,
            "handoffs were not bounded"
        );
        tokio::time::sleep(TRANSIENT_HANDOFF * 2).await;
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.bytes_pinned(), 0);
        assert!(store.temp.read().is_empty());
        assert_eq!(store.entries(), 0);
        assert_eq!(store.tags(), 0);
    }
}
