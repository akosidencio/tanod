use super::tests::{cache_cfg, meta};
use super::*;
use pingora_cache::trace::Span;
use std::time::Duration;

fn store(policy: Eviction) -> &'static BoundedStore {
    BoundedStore::new(&CacheDefaults {
        eviction: policy,
        ..cache_cfg(1 << 20)
    })
}

async fn invalidate(store: &'static BoundedStore, key: &CacheKey, scope: usize) {
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
                    &Span::inactive().handle(),
                )
                .await
                .unwrap();
        }
    }
}

fn tagged_meta(transient: bool) -> CacheMeta {
    let mut meta = meta();
    meta.response_header_mut()
        .insert_header("x-tanod-cache-tags", "product")
        .unwrap();
    if transient {
        meta.response_header_mut()
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
    }
    meta
}

// Matrix covers every purge API, ordinary and exact-write-tag attachments,
// multiple followers, coalescing-only responses, and both retention policies.
#[tokio::test(start_paused = true)]
async fn purge_closes_attachment_but_existing_followers_keep_streaming() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        for scope in 0..4 {
            for transient in [false, true] {
                let store = store(policy);
                let key = CacheKey::new("page", "/page");
                let span = Span::inactive().handle();
                let mut writer = store
                    .get_miss_handler(&key, &tagged_meta(transient), &span)
                    .await
                    .unwrap();
                let tag = writer.streaming_write_tag().unwrap().to_vec();
                writer
                    .write_body(Bytes::from_static(b"first"), false)
                    .await
                    .unwrap();
                let mut followers = Vec::new();
                for n in 0..4 {
                    let (_, mut follower) = if n % 2 == 0 {
                        store.lookup(&key, &span).await.unwrap().unwrap()
                    } else {
                        store
                            .lookup_streaming_write(&key, Some(&tag), &span)
                            .await
                            .unwrap()
                            .unwrap()
                    };
                    assert_eq!(follower.read_body().await.unwrap().unwrap(), "first");
                    followers.push(follower);
                }
                invalidate(store, &key, scope).await;
                assert!(store.lookup(&key, &span).await.unwrap().is_none());
                assert!(
                    store
                        .lookup_streaming_write(&key, Some(&tag), &span)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(store.bytes_used() > 0, "live writer lost its reservation");
                writer
                    .write_body(Bytes::from_static(b"second"), false)
                    .await
                    .unwrap();
                for follower in &mut followers {
                    // Must arrive before finish; buffering would hang this test.
                    assert_eq!(follower.read_body().await.unwrap().unwrap(), "second");
                }
                writer
                    .write_body(Bytes::from_static(b"last"), true)
                    .await
                    .unwrap();
                writer.finish().await.unwrap();
                for follower in &mut followers {
                    assert_eq!(follower.read_body().await.unwrap().unwrap(), "last");
                    assert!(follower.read_body().await.unwrap().is_none());
                }
                assert_eq!(store.bytes_used(), 0);
                assert!(store.temp.read().is_empty());
                assert_eq!(store.entries(), 0);
                assert_eq!(store.tags(), 0);
            }
        }
    }
}

#[tokio::test]
async fn purge_between_pingora_registration_and_leader_handoff_is_safe() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        let store = store(policy);
        let key = CacheKey::new("leader", "/page");
        let span = Span::inactive().handle();
        let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        let tag = writer.streaming_write_tag().unwrap().to_vec();
        store.purge_all();
        assert!(store.lookup(&key, &span).await.unwrap().is_none());
        // Mirrors Pingora's immediate lookup after get_miss_handler. This
        // transfers the attachment reserved at registration, not a new one.
        let (_, mut leader) = store
            .lookup_streaming_write(&key, Some(&tag), &span)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .lookup_streaming_write(&key, Some(&tag), &span)
                .await
                .unwrap()
                .is_none()
        );
        writer
            .write_body(Bytes::from_static(b"shell"), false)
            .await
            .unwrap();
        assert_eq!(leader.read_body().await.unwrap().unwrap(), "shell");
        writer
            .write_body(Bytes::from_static(b" tail"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert_eq!(leader.read_body().await.unwrap().unwrap(), " tail");
        assert!(leader.read_body().await.unwrap().is_none());
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.entries(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn late_retired_transient_writer_leaves_no_handoff() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        let store = store(policy);
        store.activate_namespace(1);
        let key = CacheNamespace(1).key("late".into(), "/page");
        store.activate_namespace(2);
        let span = Span::inactive().handle();
        let mut writer = store
            .get_miss_handler(&key, &tagged_meta(true), &span)
            .await
            .unwrap();
        let tag = writer.streaming_write_tag().unwrap().to_vec();
        let (_, mut reader) = store
            .lookup_streaming_write(&key, Some(&tag), &span)
            .await
            .unwrap()
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"old transient"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert_eq!(reader.read_body().await.unwrap().unwrap(), "old transient");
        assert!(reader.read_body().await.unwrap().is_none());
        assert!(store.lookup(&key, &span).await.unwrap().is_none());
        assert_eq!(store.bytes_used(), 0);
        assert!(store.temp.read().is_empty());
    }
}

#[tokio::test]
async fn cancellation_after_purge_wakes_waiting_followers_and_releases_once() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        let store = store(policy);
        let key = CacheKey::new("cancel", "/page");
        let span = Span::inactive().handle();
        let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        writer
            .write_body(Bytes::from_static(b"first"), false)
            .await
            .unwrap();
        let (_, mut follower) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert_eq!(follower.read_body().await.unwrap().unwrap(), "first");
        let (waiting, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let read = follower.read_body();
            tokio::pin!(read);
            // Explicitly poll the read to Pending before canceling the writer.
            std::future::poll_fn(|cx| {
                assert!(read.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            waiting.send(()).unwrap();
            read.await
        });
        ready.await.unwrap();
        store.purge_all();
        drop(writer);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(store.bytes_used(), 0);
        assert!(store.temp.read().is_empty());
        assert_eq!(store.purge_all(), Purged::default());
    }
}

// Force both serializations of completion versus purge. Channels ensure
// the asserted ordering instead of hoping a scheduler creates a race.
#[tokio::test]
async fn completion_and_purge_in_both_orders_never_resurrect_a_fill() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        for purge_first in [false, true] {
            let store = store(policy);
            let key = CacheKey::new("race", "/page");
            let span = Span::inactive().handle();
            let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
            writer
                .write_body(Bytes::from_static(b"body"), false)
                .await
                .unwrap();
            let (go, wait) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                wait.await.unwrap();
                writer.finish().await.unwrap();
            });
            if purge_first {
                store.purge_all();
                go.send(()).unwrap();
                task.await.unwrap();
            } else {
                go.send(()).unwrap();
                task.await.unwrap();
                store.purge_all();
            }
            assert!(store.lookup(&key, &span).await.unwrap().is_none());
            assert_eq!(store.bytes_used(), 0);
            assert!(store.temp.read().is_empty());
        }
    }
}

#[tokio::test]
async fn a_post_purge_replacement_survives_old_writer_completion() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        let store = store(policy);
        let key = CacheKey::new("replace", "/page");
        let span = Span::inactive().handle();
        let mut old = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        old.write_body(Bytes::from_static(b"old"), false)
            .await
            .unwrap();
        store.purge_all();
        let mut new = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
        new.write_body(Bytes::from_static(b"new"), true)
            .await
            .unwrap();
        new.finish().await.unwrap();
        let retained = store.bytes_used();
        old.finish().await.unwrap();
        assert!(store.bytes_used() < retained);
        let (_, mut hit) = store.lookup(&key, &span).await.unwrap().unwrap();
        assert_eq!(hit.read_body().await.unwrap().unwrap(), "new");
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }
}

// Actual threads exercise competing map/lock operations. The barrier
// creates contention; the post-purge lookup is ordered by task completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn competing_lookup_completion_and_purge_preserve_accounting() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        for _ in 0..32 {
            let store = store(policy);
            let key = CacheKey::new("contended", "/page");
            let span = Span::inactive().handle();
            let mut writer = store.get_miss_handler(&key, &meta(), &span).await.unwrap();
            writer
                .write_body(Bytes::from_static(b"body"), false)
                .await
                .unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let b = barrier.clone();
            let finish = tokio::spawn(async move {
                b.wait().await;
                writer.finish().await.unwrap();
            });
            let b = barrier.clone();
            let purge = tokio::spawn(async move {
                b.wait().await;
                store.purge_all();
            });
            barrier.wait().await;
            let attached = store.lookup(&key, &span).await.unwrap();
            finish.await.unwrap();
            purge.await.unwrap();
            if let Some((_, mut follower)) = attached {
                assert_eq!(follower.read_body().await.unwrap().unwrap(), "body");
                assert!(follower.read_body().await.unwrap().is_none());
            }
            assert!(store.lookup(&key, &span).await.unwrap().is_none());
            assert_eq!(store.bytes_used(), 0);
            assert!(store.temp.read().is_empty());
            assert_eq!(store.tags(), 0);
        }
    }
}

// A frozen handoff clock separates a genuine transition miss from legitimate
// expiry. The OS thread competes with real lookups while the Tokio test keeps
// virtual time fixed; no scheduling delay is used as synchronization.
#[tokio::test(start_paused = true)]
async fn lookup_during_transient_completion_cannot_observe_a_handoff_gap() {
    for policy in [Eviction::Clock, Eviction::Tinyufo] {
        let store = store(policy);
        let key = CacheKey::new("handoff-race", "/page");
        let span = Span::inactive().handle();
        let mut writer = store
            .get_miss_handler(&key, &tagged_meta(true), &span)
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
        for _ in 0..1000 {
            let (_, mut hit) = store
                .lookup(&key, &span)
                .await
                .unwrap()
                .expect("completion exposed a temporary hole in the handoff");
            assert_eq!(hit.read_body().await.unwrap().unwrap(), "complete");
        }
        thread.join().unwrap();
        // Let the cleanup task register its timer, then expire it explicitly.
        tokio::task::yield_now().await;
        tokio::time::advance(TRANSIENT_HANDOFF).await;
        tokio::task::yield_now().await;
        assert!(store.lookup(&key, &span).await.unwrap().is_none());
        assert_eq!(store.bytes_used(), 0);
    }
}
