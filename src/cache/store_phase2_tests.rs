//! Memory exhaustion at every point in a fill's life.
//!
//! The contract under test, from the module documentation of `store`: a fill
//! that cannot be registered is refused without side effects so Pingora can
//! serve the response uncached; a fill that runs out mid-body is abandoned and
//! finishes streaming from a bounded headroom without ever being stored; a
//! fill that exhausts the headroom too is refused, hidden from new readers,
//! and ends its existing readers with an error rather than a hang. Through all
//! of it the logical total never passes budget plus headroom, and every byte
//! is released exactly once.

use super::tests::{cache_cfg, meta};
use super::*;
use pingora_cache::trace::Span;
use std::time::Duration;

const POLICIES: [Eviction; 2] = [Eviction::Clock, Eviction::Tinyufo];

fn store(policy: Eviction, budget: usize, max_body: usize) -> &'static BoundedStore {
    BoundedStore::new(&CacheDefaults {
        eviction: policy,
        max_body_size: crate::config::units::Bytes(max_body as u64),
        ..cache_cfg(budget)
    })
}

fn span() -> SpanHandle {
    Span::inactive().handle()
}

fn key(path: &str) -> CacheKey {
    CacheKey::new(path, path)
}

/// Metadata carrying `header_bytes` of incompressible header value. Pingora
/// compresses serialised headers, so a repeated byte would shrink to nothing.
fn padded_meta(header_bytes: usize) -> CacheMeta {
    let mut state: u32 = 0x9e37_79b9;
    let noise: String = (0..header_bytes)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            char::from(b'!' + (state % 94) as u8)
        })
        .collect();
    let mut meta = meta();
    meta.response_header_mut()
        .insert_header("x-pad", noise)
        .unwrap();
    meta
}

fn body(byte: u8, len: usize) -> Bytes {
    Bytes::from(vec![byte; len])
}

async fn complete(store: &'static BoundedStore, path: &str, len: usize) {
    let mut writer = store
        .get_miss_handler(&key(path), &meta(), &span())
        .await
        .unwrap();
    writer.write_body(body(b'x', len), true).await.unwrap();
    writer.finish().await.unwrap();
}

/// A fill registered the way Pingora registers one: the handler, then the
/// leader's own reader through the exact write tag.
async fn start(store: &'static BoundedStore, path: &str) -> (MissHandler, HitHandler) {
    let writer = store
        .get_miss_handler(&key(path), &meta(), &span())
        .await
        .unwrap();
    let tag = writer.streaming_write_tag().unwrap().to_vec();
    let (_, leader) = store
        .lookup_streaming_write(&key(path), Some(&tag), &span())
        .await
        .unwrap()
        .unwrap();
    (writer, leader)
}

async fn follow(store: &'static BoundedStore, path: &str) -> Option<HitHandler> {
    store
        .lookup(&key(path), &span())
        .await
        .unwrap()
        .map(|(_, reader)| reader)
}

async fn drain(reader: &mut HitHandler) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = reader.read_body().await? {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn is_stored(store: &BoundedStore, path: &str) -> bool {
    store.cached.read().contains_key(&key(path).combined())
}

fn stored_bytes(store: &BoundedStore) -> usize {
    store.cached.read().values().map(Stored::size).sum()
}

/// After every fill has ended, only completed entries may remain accounted.
fn assert_settled(store: &BoundedStore, context: &str) {
    assert_eq!(store.bytes_pinned(), 0, "{context}: pinned bytes leaked");
    assert_eq!(
        store.bytes_used(),
        stored_bytes(store),
        "{context}: accounting disagrees with the stored entries"
    );
    assert!(
        store.temp.read().is_empty(),
        "{context}: a fill was left behind"
    );
}

fn assert_bounded(store: &BoundedStore, context: &str) {
    let bound = store.limit() + store.headroom();
    assert!(
        store.bytes_used() <= bound,
        "{context}: {} bytes used against a bound of {bound}",
        store.bytes_used()
    );
    assert!(
        store.bytes_pinned() <= bound,
        "{context}: pinned past the bound"
    );
}

#[tokio::test]
async fn metadata_larger_than_the_budget_is_refused_without_side_effects() {
    for policy in POLICIES {
        let store = store(policy, 1024, 1024);
        complete(store, "/kept", 50).await;
        let before = store.bytes_used();

        let refused = store
            .get_miss_handler(&key("/big"), &padded_meta(2048), &span())
            .await;
        assert!(refused.is_err(), "{policy:?}");
        assert!(is_stored(store, "/kept"), "{policy:?}: a refusal evicted");
        assert_eq!(store.bytes_used(), before, "{policy:?}");
        assert_eq!(store.memory_pressure().uncached_fallback, 1, "{policy:?}");
        assert_eq!(store.memory_pressure().fill_abandoned, 0, "{policy:?}");
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// The reservation is decided from what fills hold, because only completed
/// entries can be evicted. One that cannot fit beside the fills must not empty
/// the cache on its way to failing anyway.
#[tokio::test]
async fn a_doomed_metadata_reservation_does_not_empty_the_cache() {
    for policy in POLICIES {
        let store = store(policy, 4096, 4096);
        complete(store, "/kept", 500).await;
        let (mut pin, _leader) = start(store, "/pin").await;
        // Fill the budget exactly to within 64 bytes, without evicting.
        let room = store.limit() - store.bytes_used() - 64;
        pin.write_body(body(b'p', room), false).await.unwrap();
        assert!(
            is_stored(store, "/kept"),
            "{policy:?}: the pin itself evicted"
        );
        assert!(!store.accepting_fills(1100), "{policy:?}");

        let refused = store
            .get_miss_handler(&key("/late"), &padded_meta(1000), &span())
            .await;
        assert!(refused.is_err(), "{policy:?}");
        assert!(
            is_stored(store, "/kept"),
            "{policy:?}: emptied the cache for nothing"
        );

        drop(pin);
        assert!(store.accepting_fills(1100), "{policy:?}");
        let accepted = store
            .get_miss_handler(&key("/late"), &padded_meta(1000), &span())
            .await;
        assert!(accepted.is_ok(), "{policy:?}");
        drop(accepted);
        assert_settled(store, &format!("{policy:?}"));
    }
}

#[tokio::test]
async fn completed_entries_never_make_the_store_refuse_new_fills() {
    for policy in POLICIES {
        let store = store(policy, 4096, 1024);
        for n in 0..8 {
            complete(store, &format!("/full/{n}"), 600).await;
        }
        assert!(
            store.accepting_fills(512),
            "{policy:?}: evictable bytes counted as full"
        );
        let (mut writer, _leader) = start(store, "/new").await;
        writer.write_body(body(b'n', 1000), true).await.unwrap();
        writer.finish().await.unwrap();
        assert!(store.bytes_used() <= store.limit(), "{policy:?}");
        assert_eq!(
            store.memory_pressure(),
            MemoryPressure::default(),
            "{policy:?}"
        );
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// The central fallback. Every reader — the leader, a follower attached
/// before the budget ran out and one attached after — gets the complete,
/// byte-identical body and a clean end; nothing is stored; completed entries
/// that did not need to go were not evicted; the reservation is returned.
#[tokio::test]
async fn a_fill_that_runs_out_mid_body_completes_for_every_reader_and_is_not_stored() {
    for policy in POLICIES {
        let store = store(policy, 4096, 4096);
        complete(store, "/old", 1000).await;
        let (mut writer, mut leader) = start(store, "/page").await;
        writer.write_body(body(b'a', 2000), false).await.unwrap();
        let mut early = follow(store, "/page").await.unwrap();

        writer.write_body(body(b'b', 3000), false).await.unwrap();
        assert!(
            store.bytes_used() > store.limit(),
            "{policy:?}: no headroom used"
        );
        assert_bounded(store, &format!("{policy:?}"));
        assert_eq!(store.memory_pressure().fill_abandoned, 1, "{policy:?}");
        assert!(
            is_stored(store, "/old"),
            "{policy:?}: headroom evicted needlessly"
        );

        let mut late = follow(store, "/page")
            .await
            .expect("an abandoned fill still coalesces");
        writer
            .write_body(Bytes::from_static(b"end"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();

        let mut expected = vec![b'a'; 2000];
        expected.extend_from_slice(&[b'b'; 3000]);
        expected.extend_from_slice(b"end");
        for (name, reader) in [
            ("leader", &mut leader),
            ("early", &mut early),
            ("late", &mut late),
        ] {
            assert_eq!(drain(reader).await.unwrap(), expected, "{policy:?} {name}");
        }
        assert!(
            !is_stored(store, "/page"),
            "{policy:?}: an over-budget fill was stored"
        );
        assert!(
            follow(store, "/page").await.is_none(),
            "{policy:?}: lingering handoff"
        );
        assert_eq!(store.memory_pressure().memory_exhausted, 0, "{policy:?}");
        assert_settled(store, &format!("{policy:?}"));
    }
}

#[tokio::test]
async fn a_budget_exhausted_before_the_first_body_byte_still_serves_the_response() {
    for policy in POLICIES {
        let store = store(policy, 2048, 2048);
        let (mut hog, _hog_leader) = start(store, "/hog").await;
        hog.write_body(body(b'h', 1600), false).await.unwrap();

        let (mut writer, mut leader) = start(store, "/page").await;
        writer.write_body(body(b'q', 900), true).await.unwrap();
        assert_eq!(store.memory_pressure().fill_abandoned, 1, "{policy:?}");
        writer.finish().await.unwrap();
        assert_eq!(
            drain(&mut leader).await.unwrap(),
            vec![b'q'; 900],
            "{policy:?}"
        );
        assert!(!is_stored(store, "/page"), "{policy:?}");

        drop(hog);
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// Past the headroom the fill fails. The readers it has must end with an
/// error — Pingora then aborts the response — and must not wait forever; no
/// new reader may attach between the failure and the handler's drop.
#[tokio::test]
async fn exhausting_the_headroom_ends_the_fill_and_its_readers_without_hanging() {
    for policy in POLICIES {
        let store = store(policy, 2048, 1024);
        let (mut writer, mut leader) = start(store, "/page").await;
        writer.write_body(body(b'a', 1500), false).await.unwrap();
        let mut follower = follow(store, "/page").await.unwrap();
        writer.write_body(body(b'b', 1000), false).await.unwrap();
        assert_eq!(store.memory_pressure().fill_abandoned, 1, "{policy:?}");

        assert!(
            writer.write_body(body(b'c', 1000), false).await.is_err(),
            "{policy:?}: wrote past budget plus headroom"
        );
        assert_bounded(store, &format!("{policy:?}"));
        assert_eq!(store.memory_pressure().memory_exhausted, 1, "{policy:?}");
        assert!(
            follow(store, "/page").await.is_none(),
            "{policy:?}: a reader attached to a failed fill"
        );

        drop(writer);
        for reader in [&mut leader, &mut follower] {
            let ended = tokio::time::timeout(Duration::from_secs(5), drain(reader))
                .await
                .expect("a reader of a failed fill hung");
            assert!(ended.is_err(), "{policy:?}: a truncated body ended cleanly");
        }
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// One response alone can never take the store past budget plus headroom,
/// however long it streams.
#[tokio::test]
async fn an_oversized_body_is_cut_off_at_the_bound() {
    for policy in POLICIES {
        let store = store(policy, 2048, 1024);
        let (mut writer, _leader) = start(store, "/huge").await;
        let mut written = 0;
        let refused = loop {
            match writer.write_body(body(b'z', 256), false).await {
                Ok(()) => written += 256,
                Err(_) => break written,
            }
            assert_bounded(store, &format!("{policy:?}"));
        };
        assert!(refused > 2048, "{policy:?}: the headroom was not used");
        assert!(refused <= 2048 + 1024, "{policy:?}");
        drop(writer);
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// A client that disconnects mid-fill drops the handler. Under pressure this
/// must still return the budget and headroom bytes exactly once.
#[tokio::test]
async fn cancellation_at_every_stage_releases_exactly_once() {
    for policy in POLICIES {
        for stage in 0..4 {
            let store = store(policy, 2048, 1024);
            complete(store, "/kept", 100).await;
            let (mut writer, leader) = start(store, "/page").await;
            if stage >= 1 {
                writer.write_body(body(b'a', 1200), false).await.unwrap();
            }
            if stage >= 2 {
                writer.write_body(body(b'b', 1000), false).await.unwrap();
                assert_eq!(store.memory_pressure().fill_abandoned, 1);
            }
            if stage >= 3 {
                assert!(writer.write_body(body(b'c', 1000), false).await.is_err());
            }
            drop(writer);
            drop(leader);
            assert_settled(store, &format!("{policy:?} stage {stage}"));
        }
    }
}

#[tokio::test]
async fn a_purge_during_an_abandoned_fill_keeps_its_readers_and_its_accounting() {
    for policy in POLICIES {
        for scope in 0..3 {
            let store = store(policy, 2048, 2048);
            let (mut writer, mut leader) = start(store, "/page").await;
            writer.write_body(body(b'a', 1500), false).await.unwrap();
            writer.write_body(body(b'b', 1500), false).await.unwrap();
            assert_eq!(store.memory_pressure().fill_abandoned, 1);
            match scope {
                0 => drop(store.purge_paths(["/page"])),
                1 => drop(store.purge_all()),
                _ => drop(store.activate_namespace(7)),
            }
            assert!(follow(store, "/page").await.is_none() || scope == 2);
            writer
                .write_body(Bytes::from_static(b"!"), true)
                .await
                .unwrap();
            writer.finish().await.unwrap();
            let got = drain(&mut leader).await.unwrap();
            assert_eq!(got.len(), 3001, "{policy:?} scope {scope}");
            assert!(!is_stored(store, "/page"));
            assert_settled(store, &format!("{policy:?} scope {scope}"));
        }
    }
}

/// Phase 1's namespace retirement and this fallback compose: a fill from a
/// retired policy that also ran out of budget is neither stored nor leaked.
#[tokio::test]
async fn namespace_retirement_during_an_abandoned_fill() {
    for policy in POLICIES {
        let store = store(policy, 2048, 2048);
        store.activate_namespace(1);
        let pinned_key = CacheNamespace(1).key("canonical".into(), "/page");
        let mut writer = store
            .get_miss_handler(&pinned_key, &meta(), &span())
            .await
            .unwrap();
        writer.write_body(body(b'a', 1500), false).await.unwrap();
        writer.write_body(body(b'b', 1500), false).await.unwrap();
        store.activate_namespace(2);
        writer.write_body(Bytes::new(), true).await.unwrap();
        writer.finish().await.unwrap();
        assert_eq!(store.entries(), 0, "{policy:?}");
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// A coalescing-only response normally lingers in a short handoff. Abandoned,
/// it returns its over-budget bytes at once instead.
#[tokio::test]
async fn an_abandoned_transient_fill_skips_the_handoff() {
    for policy in POLICIES {
        let store = store(policy, 2048, 2048);
        let mut transient = meta();
        transient
            .response_header_mut()
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
        let mut writer = store
            .get_miss_handler(&key("/t"), &transient, &span())
            .await
            .unwrap();
        writer.write_body(body(b'a', 1500), false).await.unwrap();
        writer.write_body(body(b'b', 1500), true).await.unwrap();
        writer.finish().await.unwrap();
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// Eviction under pressure: a budgeted fill evicts completed entries to stay
/// inside the budget; an abandoned one evicts only what it needs to stay
/// inside budget plus headroom.
#[tokio::test]
async fn eviction_under_pressure_respects_each_bound() {
    for policy in POLICIES {
        let store = store(policy, 4096, 1024);
        for n in 0..3 {
            complete(store, &format!("/e/{n}"), 1000).await;
        }
        let (mut writer, mut leader) = start(store, "/page").await;
        writer.write_body(body(b'a', 1500), false).await.unwrap();
        assert!(
            store.bytes_used() <= store.limit(),
            "{policy:?}: budget broken"
        );
        writer.write_body(body(b'b', 2600), false).await.unwrap();
        assert_eq!(store.memory_pressure().fill_abandoned, 1, "{policy:?}");
        assert_bounded(store, &format!("{policy:?}"));
        writer.write_body(Bytes::new(), true).await.unwrap();
        writer.finish().await.unwrap();
        assert_eq!(drain(&mut leader).await.unwrap().len(), 4100, "{policy:?}");
        assert!(
            store.bytes_used() <= store.limit(),
            "{policy:?}: left over budget"
        );
        assert_settled(store, &format!("{policy:?}"));
    }
}

/// Many fills racing for one budget, interleaved at every write. Whatever mix
/// of stored, abandoned, failed and refused results, the bound holds at every
/// step and the accounting settles to exactly the stored entries.
#[tokio::test]
async fn simultaneous_fills_share_one_budget_and_one_headroom() {
    for policy in POLICIES {
        let store = store(policy, 8192, 2048);
        let mut tasks = Vec::new();
        for n in 0..10 {
            tasks.push(tokio::spawn(async move {
                let path = format!("/race/{n}");
                let Ok(mut writer) = store.get_miss_handler(&key(&path), &meta(), &span()).await
                else {
                    return "refused";
                };
                for chunk in 0..4 {
                    tokio::task::yield_now().await;
                    let last = chunk == 3;
                    if writer.write_body(body(b'r', 700), last).await.is_err() {
                        assert_bounded(store, &path);
                        return "failed";
                    }
                    assert_bounded(store, &path);
                }
                writer.finish().await.unwrap();
                "finished"
            }));
        }
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.unwrap());
        }
        let pressure = store.memory_pressure();
        assert!(
            pressure.fill_abandoned > 0,
            "{policy:?}: the race never hit the budget"
        );
        let refused = outcomes.iter().filter(|o| **o == "refused").count() as u64;
        let failed = outcomes.iter().filter(|o| **o == "failed").count() as u64;
        assert_eq!(pressure.uncached_fallback, refused, "{policy:?}");
        assert_eq!(pressure.memory_exhausted, failed, "{policy:?}");
        assert!(store.bytes_used() <= store.limit(), "{policy:?}");
        assert_settled(store, &format!("{policy:?}"));
    }
}
