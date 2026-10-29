//! Forced schedules of the real store, plus a small Loom model of the
//! admit-before-remove / double-lookup protocol. Hooks compile out of releases.
use super::phase3_tests::{drain, span, store};
use super::tests::meta;
use super::*;
use std::sync::mpsc;
use std::time::Duration;

type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

#[derive(Default)]
pub(super) struct Hooks {
    pub lookup_missed: Hook,
    pub before_completion: Hook,
    pub admitted: Hook,
}

impl Hooks {
    pub fn run(hook: &Hook) {
        let callback = hook.lock().take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// A one-shot gate, bounded so a failed assertion cannot hang the test suite.
fn gate(hook: &Hook) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, wait) = mpsc::channel();
    let (resume, go) = mpsc::channel();
    *hook.lock() = Some(Box::new(move || {
        entered.send(()).unwrap();
        go.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    (wait, resume)
}

fn arrived(wait: mpsc::Receiver<()>) {
    wait.recv_timeout(Duration::from_secs(10)).unwrap();
}

async fn writer(store: &'static BoundedStore, key: &CacheKey) -> MissHandler {
    let mut writer = store.get_miss_handler(key, &meta(), &span()).await.unwrap();
    writer
        .write_body(Bytes::from_static(b"whole body"), false)
        .await
        .unwrap();
    writer
}

fn finish(writer: MissHandler) -> std::thread::JoinHandle<()> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || runtime.block_on(writer.finish()).map(|_| ()).unwrap())
}

const POLICIES: [Eviction; 2] = [Eviction::Clock, Eviction::Tinyufo];

#[tokio::test]
async fn completion_between_the_two_map_reads_is_visible() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        let key = CacheKey::new("gap", "/gap");
        let writer = writer(store, &key).await;
        let (wait, resume) = gate(&store.hooks.lookup_missed);
        let runtime = tokio::runtime::Handle::current();
        let read_key = key.clone();
        let lookup = std::thread::spawn(move || {
            runtime.block_on(async {
                let (_, mut hit) = store.lookup(&read_key, &span()).await.unwrap().unwrap();
                drain(&mut hit).await
            })
        });
        arrived(wait); // lookup has observed an empty completed map
        writer.finish().await.unwrap(); // both admission AND removal finish
        assert!(store.temp.read().is_empty());
        resume.send(()).unwrap();
        assert_eq!(lookup.join().unwrap(), b"whole body");
        assert_eq!(store.bytes_pinned(), 0);
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }
}

#[tokio::test]
async fn purge_and_retirement_before_completion_block_admission() {
    for policy in POLICIES {
        for retire in [false, true] {
            let store = store(policy, 1 << 20);
            store.activate_namespace(1);
            let key = CacheNamespace(1).key("old".into(), "/old");
            let writer = writer(store, &key).await;
            let (_, mut follower) = store.lookup(&key, &span()).await.unwrap().unwrap();
            let (wait, resume) = gate(&store.hooks.before_completion);
            let completion = finish(writer);
            arrived(wait); // body frozen, lifecycle guard not acquired yet
            if retire {
                store.activate_namespace(2);
            } else {
                store.purge_all();
            }
            assert!(store.lookup(&key, &span()).await.unwrap().is_none());
            resume.send(()).unwrap();
            completion.join().unwrap();
            assert_eq!(drain(&mut follower).await, b"whole body");
            assert_eq!(store.entries(), 0);
            assert!(store.temp.read().is_empty());
            assert_eq!(store.bytes_pinned(), 0);
            assert_eq!(store.bytes_used(), 0);
        }
    }
}

#[tokio::test]
async fn purge_and_retirement_after_admission_remove_the_completed_entry() {
    for policy in POLICIES {
        for retire in [false, true] {
            let store = store(policy, 1 << 20);
            store.activate_namespace(1);
            let key = CacheNamespace(1).key("old".into(), "/old");
            let writer = writer(store, &key).await;
            let (wait, resume) = gate(&store.hooks.admitted);
            let completion = finish(writer);
            arrived(wait); // both maps contain the response; lifecycle held shared
            assert!(store.lifecycle.try_write().is_none());
            let (_, mut follower) = store.lookup(&key, &span()).await.unwrap().unwrap();
            resume.send(()).unwrap();
            completion.join().unwrap();
            if retire {
                store.activate_namespace(2);
            } else {
                store.purge_all();
            }
            assert!(store.lookup(&key, &span()).await.unwrap().is_none());
            assert_eq!(drain(&mut follower).await, b"whole body");
            assert_eq!(store.bytes_used(), 0);
            assert_eq!(store.bytes_pinned(), 0);
        }
    }
}

#[tokio::test]
async fn late_retired_fill_only_transfers_its_reserved_leader() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        store.activate_namespace(1);
        let key = CacheNamespace(1).key("late".into(), "/late");
        store.activate_namespace(2);
        let mut writer = writer(store, &key).await;
        let tag = writer.streaming_write_tag().unwrap().to_vec();
        assert!(store.lookup(&key, &span()).await.unwrap().is_none());
        let (_, mut leader) = store
            .lookup_streaming_write(&key, Some(&tag), &span())
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .lookup_streaming_write(&key, Some(&tag), &span())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(leader.read_body().await.unwrap().unwrap(), "whole body");
        writer
            .write_body(Bytes::from_static(b" tail"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
        assert_eq!(drain(&mut leader).await, b" tail");
        assert!(store.temp.read().is_empty());
        assert_eq!(store.bytes_used(), 0);
    }
}

#[tokio::test]
async fn eviction_between_admission_and_removal_preserves_attached_readers() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        let key = CacheKey::new("evict", "/evict");
        let writer = writer(store, &key).await;
        let (_, mut before) = store.lookup(&key, &span()).await.unwrap().unwrap();
        let (wait, resume) = gate(&store.hooks.admitted);
        let completion = finish(writer);
        arrived(wait);
        // Reserve the entire budget: eviction must remove the admitted entry.
        assert_eq!(
            store.reserve_fill(store.limit(), true, false),
            Grant::Budget
        );
        assert_eq!(store.entries(), 0);
        let (_, mut during) = store.lookup(&key, &span()).await.unwrap().unwrap();
        resume.send(()).unwrap();
        completion.join().unwrap();
        assert!(store.lookup(&key, &span()).await.unwrap().is_none()); // genuine eviction
        assert_eq!(drain(&mut before).await, b"whole body");
        assert_eq!(drain(&mut during).await, b"whole body");
        store.release(store.limit());
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.bytes_pinned(), 0);
    }
}

#[tokio::test]
async fn dropping_an_unpolled_finish_releases_the_writer_once() {
    for policy in POLICIES {
        let store = store(policy, 1 << 20);
        let key = CacheKey::new("cancel-finish", "/cancel-finish");
        let writer = writer(store, &key).await;
        let (_, mut follower) = store.lookup(&key, &span()).await.unwrap().unwrap();
        assert_eq!(follower.read_body().await.unwrap().unwrap(), "whole body");
        // finish has no suspension points once polled: cancellation can occur
        // before its first poll, or after the entire synchronous transition.
        drop(writer.finish());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), follower.read_body())
                .await
                .unwrap()
                .is_err()
        );
        assert!(store.temp.read().is_empty());
        assert_eq!(store.bytes_used(), 0);
        assert_eq!(store.bytes_pinned(), 0);
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousands_of_completions_on_distinct_keys_balance_reservations() {
    for policy in POLICIES {
        let store = store(policy, 8 << 20);
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for worker in 0..8 {
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                for round in 0..300 {
                    let name = format!("/{worker}/{round}");
                    let key = CacheKey::new(name.clone(), name);
                    let writer = writer(store, &key).await;
                    let (_, mut follower) = store.lookup(&key, &span()).await.unwrap().unwrap();
                    tokio::task::yield_now().await;
                    writer.finish().await.unwrap();
                    assert_eq!(drain(&mut follower).await, b"whole body");
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert!(store.temp.read().is_empty());
        assert_eq!(store.bytes_pinned(), 0);
        assert_eq!(
            store.bytes_used(),
            store
                .cached
                .read()
                .values()
                .map(Stored::size)
                .sum::<usize>()
        );
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }
}

#[test]
fn loom_completion_lookup_and_purge_model() {
    // Abstracts map presence, not body/metadata, eviction or TinyUFO internals.
    // Each lock corresponds to the real store's lock; no sequentially
    // consistent flag is substituted for the map synchronization.
    loom::model(|| {
        use loom::sync::{Arc, RwLock};
        use loom::thread;
        let lifecycle = Arc::new(RwLock::new(false)); // purged
        let cached = Arc::new(RwLock::new(false));
        let temp = Arc::new(RwLock::new(true));
        let completion = {
            let (lifecycle, cached, temp) = (lifecycle.clone(), cached.clone(), temp.clone());
            thread::spawn(move || {
                let purged = lifecycle.read().unwrap();
                if !*purged {
                    *cached.write().unwrap() = true;
                }
                *temp.write().unwrap() = false;
            })
        };
        let purge = {
            let (lifecycle, cached, temp) = (lifecycle.clone(), cached.clone(), temp.clone());
            thread::spawn(move || {
                let mut purged = lifecycle.write().unwrap();
                *temp.write().unwrap() = false;
                *cached.write().unwrap() = false;
                *purged = true;
            })
        };
        let purged = lifecycle.read().unwrap();
        let first = *cached.read().unwrap();
        let found = first || *temp.read().unwrap() || *cached.read().unwrap();
        assert!(found || *purged, "normal completion became invisible");
        assert!(!*purged || !found, "post-purge attachment");
        drop(purged);
        completion.join().unwrap();
        purge.join().unwrap();
        assert!(!*cached.read().unwrap(), "stale resurrection");
    });
}
