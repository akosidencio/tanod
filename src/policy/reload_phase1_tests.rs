use super::tests::setup;
use super::*;
use crate::cache::key::{CacheNamespace, KeyBuilder};
use crate::cache::store::BoundedStore;
use crate::classifier::RequestMetadata;
use crate::config::schema::{CacheDefaults, Eviction};
use bytes::Bytes;
use pingora_cache::CacheMeta;
use pingora_cache::key::{CacheHashKey, CacheKey};
use pingora_cache::storage::{MissHandler, Storage};
use pingora_cache::trace::Span;
use pingora_http::ResponseHeader;
use std::time::{Duration, SystemTime};

const PUBLIC: &str = r#"
version: 1
origin:
  upstreams: ["a:3000"]
  concurrency:
    max: 100
routes:
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 60s
"#;

fn key(policy: &PolicySnapshot, path: &str) -> CacheKey {
    let method = http::Method::GET;
    let headers = http::HeaderMap::new();
    let request = RequestMetadata {
        method: &method,
        host: "example.test",
        path,
        query: Some("sort=price"),
        headers: &headers,
    };
    let route = policy.resolve(request.host, path, &method).unwrap();
    let cache = route.config.cache.as_ref();
    let variants = cache
        .and_then(|c| c.vary.as_ref())
        .map(|v| v.headers.clone())
        .unwrap_or_default();
    let canonical = KeyBuilder {
        scheme: "https",
        forwarded_host: None,
        query_policy: cache.and_then(|c| c.query.as_ref()),
        variant_headers: &variants,
        deployment: policy.config.deployment.id.as_deref(),
    }
    .build(&request);
    CacheNamespace(policy.cache_namespace).key(canonical.canonical_string(), path)
}

async fn writer(store: &'static BoundedStore, key: &CacheKey, body: &'static [u8]) -> MissHandler {
    let now = SystemTime::now();
    let meta = CacheMeta::new(
        now + Duration::from_secs(60),
        now,
        0,
        0,
        ResponseHeader::build(200, None).unwrap(),
    );
    let mut writer = store
        .get_miss_handler(key, &meta, &Span::inactive().handle())
        .await
        .unwrap();
    writer
        .write_body(Bytes::from_static(body), false)
        .await
        .unwrap();
    writer
}

async fn hit(store: &'static BoundedStore, key: &CacheKey) -> Option<Bytes> {
    match store.lookup(key, &Span::inactive().handle()).await.unwrap() {
        Some((_, mut hit)) => hit.read_body().await.unwrap(),
        None => None,
    }
}

fn changes() -> Vec<String> {
    vec![
        PUBLIC
            .replace("public_ssr", "private_dynamic")
            .replace("override_origin: true", "override_origin: false"),
        PUBLIC.replace("override_origin: true", "override_origin: false"),
        PUBLIC.replace("max: 60s", "max: 2s"),
        format!("{PUBLIC}      query:\n        mode: include\n        keys: [sort]\n"),
        format!("{PUBLIC}      vary:\n        headers: [accept-language]\n"),
        format!("{PUBLIC}deployment:\n  id: build-2\n"),
        format!("{PUBLIC}cache:\n  enabled: false\n"),
        format!("{PUBLIC}    coalesce:\n      override_origin: true\n"),
        format!("{PUBLIC}      stale_if_error: 1s\n"),
        format!("{PUBLIC}      stale_while_revalidate: 1s\n"),
        PUBLIC.replace("match: \"/products/**\"", "match: \"/**\""),
    ]
}

#[tokio::test]
async fn sharing_changes_isolate_completed_active_and_late_old_fills() {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        for changed in changes() {
            let store = BoundedStore::new(&CacheDefaults {
                eviction,
                ..Default::default()
            });
            let (path, reloader, policy) = setup(PUBLIC);
            let reloader = reloader.with_store(store);
            let pinned = policy.load_full();
            let old_key = key(&pinned, "/products/one");
            writer(store, &old_key, b"old cached")
                .await
                .finish()
                .await
                .unwrap();
            assert_eq!(hit(store, &old_key).await.unwrap(), "old cached");
            let active_key = key(&pinned, "/products/two");
            let active = writer(store, &active_key, b"old active").await;
            let (_, mut follower) = store
                .lookup(&active_key, &Span::inactive().handle())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(follower.read_body().await.unwrap().unwrap(), "old active");

            std::fs::write(&path.0, &changed).unwrap();
            reloader.reload().unwrap();
            let next = policy.load_full();
            assert_ne!(pinned.cache_namespace, next.cache_namespace, "{changed}");
            assert_eq!(store.entries(), 0);
            assert!(store.bytes_used() > 0, "active fill lost accounting");
            let new_key = key(&next, "/products/one");
            assert!(hit(store, &new_key).await.is_none(), "{changed}");
            // A new-generation response cannot be replaced by an old finish.
            writer(store, &new_key, b"new cached")
                .await
                .finish()
                .await
                .unwrap();
            active.finish().await.unwrap();
            assert!(follower.read_body().await.unwrap().is_none());
            assert_eq!(hit(store, &new_key).await.unwrap(), "new cached");
            assert!(hit(store, &active_key).await.is_none());

            // Pinned old request did not even register until after reload.
            // Its reserved leader can stream, but new followers cannot attach.
            // It cannot persist or leave a handoff.
            let late_key = key(&pinned, "/products/late");
            let mut late = writer(store, &late_key, b"late old").await;
            assert!(hit(store, &late_key).await.is_none());
            let tag = late.streaming_write_tag().unwrap().to_vec();
            let (_, mut late_reader) = store
                .lookup_streaming_write(&late_key, Some(&tag), &Span::inactive().handle())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(late_reader.read_body().await.unwrap().unwrap(), "late old");
            let retained = store.bytes_used();
            late.write_body(Bytes::from_static(b" tail"), true)
                .await
                .unwrap();
            late.finish().await.unwrap();
            assert_eq!(late_reader.read_body().await.unwrap().unwrap(), " tail");
            assert!(late_reader.read_body().await.unwrap().is_none());
            assert!(store.bytes_used() < retained);
            assert!(hit(store, &late_key).await.is_none());
            assert_eq!(store.entries(), 1);
            store.purge_all();
            assert_eq!(store.bytes_used(), 0);
        }
    }
}

#[tokio::test]
async fn unrelated_and_identical_reloads_keep_real_cache_hits() {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        let store = BoundedStore::new(&CacheDefaults {
            eviction,
            ..Default::default()
        });
        let (path, reloader, policy) = setup(PUBLIC);
        let reloader = reloader.with_store(store);
        let namespace = policy.load().cache_namespace;
        let old_key = key(&policy.load(), "/products/one");
        writer(store, &old_key, b"retained")
            .await
            .finish()
            .await
            .unwrap();
        let retained = store.bytes_used();
        for changed in [
            PUBLIC.to_string(),
            PUBLIC.replace("max: 100", "max: 120"),
            format!("{PUBLIC}debug_headers: true\n"),
            PUBLIC.replace("id: products", "id: renamed"),
        ] {
            std::fs::write(&path.0, changed).unwrap();
            reloader.reload().unwrap();
            assert_eq!(namespace, policy.load().cache_namespace);
            assert_eq!(
                hit(store, &key(&policy.load(), "/products/one"))
                    .await
                    .unwrap(),
                "retained"
            );
            assert_eq!(retained, store.bytes_used());
        }
        store.purge_all();
        assert_eq!(store.bytes_used(), 0);
    }
}

#[test]
fn reload_preserves_privacy_guards_and_framework_variants() {
    use crate::cache::policy::{
        Disposition, ResponseMetadata, Shareability, evaluate_request, evaluate_response,
    };
    use crate::classifier::{FrameworkAdapter, nextjs::NextJs};
    let (path, reloader, policy) = setup(PUBLIC);
    let pinned = policy.load_full();
    std::fs::write(
        &path.0,
        PUBLIC.replace("override_origin: true", "override_origin: false"),
    )
    .unwrap();
    reloader.reload().unwrap();
    let current = policy.load_full();
    for snapshot in [&pinned, &current] {
        let route = snapshot
            .resolve("example.test", "/products/one", &http::Method::GET)
            .unwrap();
        for (name, value) in [
            ("authorization", "Bearer private"),
            ("cookie", "__prerender_bypass=preview"),
            ("next-action", "action-id"),
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_static(value),
            );
            let request = RequestMetadata {
                method: &http::Method::GET,
                host: "example.test",
                path: "/products/one",
                query: None,
                headers: &headers,
            };
            let hints = NextJs.classify_request(&request);
            let class = route.declared_class().unwrap();
            assert!(matches!(
                evaluate_request(
                    &request,
                    class,
                    hints.force_bypass,
                    route.config.cache.as_ref(),
                    false,
                    true,
                    true
                ),
                Disposition::Bypass(_)
            ));
        }
        for (cookie, vary) in [(true, None), (false, Some("x-unkeyed-private"))] {
            assert!(matches!(
                evaluate_response(
                    &ResponseMetadata {
                        status: 200,
                        cache_control: Some("public, max-age=60"),
                        set_cookie: cookie,
                        vary
                    },
                    route.config.cache.as_ref(),
                    &[],
                    false,
                    false
                ),
                Shareability::NotShareable(_)
            ));
        }
        let mut keys = std::collections::HashSet::new();
        for names in [vec![], vec!["rsc"], vec!["rsc", "next-router-prefetch"]] {
            let mut headers = http::HeaderMap::new();
            for name in names {
                headers.insert(
                    http::HeaderName::from_static(name),
                    http::HeaderValue::from_static("1"),
                );
            }
            let request = RequestMetadata {
                method: &http::Method::GET,
                host: "example.test",
                path: "/products/one",
                query: None,
                headers: &headers,
            };
            let hints = NextJs.classify_request(&request);
            let variants: Vec<String> = hints.key_headers.iter().map(|h| h.to_string()).collect();
            let canonical = KeyBuilder {
                scheme: "https",
                forwarded_host: None,
                query_policy: None,
                variant_headers: &variants,
                deployment: None,
            }
            .build(&request);
            assert!(
                keys.insert(
                    CacheNamespace(snapshot.cache_namespace)
                        .key(canonical.canonical_string(), "")
                        .primary()
                )
            );
        }
    }
    // An ordinary session cookie becomes ineligible when its explicit safe
    // override is removed, even though the request still matches the route.
    let mut headers = http::HeaderMap::new();
    headers.insert("cookie", http::HeaderValue::from_static("session=private"));
    let request = RequestMetadata {
        method: &http::Method::GET,
        host: "example.test",
        path: "/products/one",
        query: None,
        headers: &headers,
    };
    for (snapshot, bypass) in [(&pinned, false), (&current, true)] {
        let route = snapshot
            .resolve(request.host, request.path, request.method)
            .unwrap();
        assert_eq!(
            matches!(
                evaluate_request(
                    &request,
                    route.declared_class().unwrap(),
                    None,
                    route.config.cache.as_ref(),
                    false,
                    true,
                    true
                ),
                Disposition::Bypass(_)
            ),
            bypass
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn rapid_reload_with_concurrent_purge_reads_and_old_completion() {
    for eviction in [Eviction::Clock, Eviction::Tinyufo] {
        let store = BoundedStore::new(&CacheDefaults {
            eviction,
            ..Default::default()
        });
        let (path, reloader, policy) = setup(PUBLIC);
        let reloader = Arc::new(reloader.with_store(store));
        let mut namespaces = std::collections::HashSet::new();
        namespaces.insert(policy.load().cache_namespace);
        for n in 0..32 {
            let old_key = key(&policy.load(), "/products/one");
            let active = writer(store, &old_key, b"old").await;
            let (_, mut attached) = store
                .lookup(&old_key, &Span::inactive().handle())
                .await
                .unwrap()
                .unwrap();
            let changed =
                PUBLIC.replace("max: 60s", if n % 2 == 0 { "max: 2s" } else { "max: 60s" });
            std::fs::write(&path.0, changed).unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let b = barrier.clone();
            let r = reloader.clone();
            let reload = tokio::spawn(async move {
                b.wait().await;
                r.reload().unwrap();
            });
            let b = barrier.clone();
            let purge = tokio::spawn(async move {
                b.wait().await;
                store.purge_all();
            });
            barrier.wait().await;
            // Either misses after invalidation or retains a valid old reader.
            let _ = hit(store, &old_key).await;
            active.finish().await.unwrap();
            reload.await.unwrap();
            purge.await.unwrap();
            assert!(
                namespaces.insert(policy.load().cache_namespace),
                "retired namespace reused"
            );
            assert_eq!(attached.read_body().await.unwrap().unwrap(), "old");
            assert!(attached.read_body().await.unwrap().is_none());
            assert!(
                hit(store, &key(&policy.load(), "/products/one"))
                    .await
                    .is_none()
            );
            assert_eq!(store.entries(), 0);
            assert_eq!(store.bytes_used(), 0);
        }
    }
}
