//! Compare production streaming-store retention and a completed-only reference.
//! This does not model concurrent fills, expiry, purges, or allocator memory.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use pingora_cache::CacheMeta;
use pingora_cache::key::CacheKey;
use pingora_cache::storage::Storage;
use pingora_cache::trace::Span;
use pingora_http::ResponseHeader;
use tanod::cache::store::BoundedStore;
use tanod::config::schema::{CacheDefaults, Eviction};
use tanod::config::units::Bytes as ByteSize;
use tinyufo::TinyUfo;

const KEYS: usize = 200;
const REQUESTS: usize = 4_000;
const BUDGET: usize = 16_000;

struct Object {
    key: CacheKey,
    meta: CacheMeta,
    body: Bytes,
    weight: u16,
}

fn workload() -> Vec<usize> {
    let mut cumulative = Vec::with_capacity(KEYS);
    let mut total = 0.0;
    for rank in 0..KEYS {
        total += 1.0 / (rank as f64 + 1.0);
        cumulative.push(total);
    }
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    (0..REQUESTS)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let pick = ((seed >> 11) as f64 / (1u64 << 53) as f64) * total;
            cumulative.partition_point(|p| *p < pick)
        })
        .collect()
}

fn objects(mixed: bool) -> Result<Vec<Arc<Object>>, Box<dyn std::error::Error>> {
    let now = SystemTime::now();
    (0..KEYS)
        .map(|key| {
            let path = format!("/p/{key:04}");
            let body_len = if mixed { 400 + (key % 8) * 600 } else { 400 };
            let meta = CacheMeta::new(
                now + Duration::from_secs(600),
                now,
                0,
                0,
                ResponseHeader::build(200, None)?,
            );
            let serialized = meta.serialize()?;
            // Same accounting as Stored::size(): metadata, body and purge path.
            let weight =
                u16::try_from(serialized.0.len() + serialized.1.len() + body_len + path.len())?;
            Ok(Arc::new(Object {
                key: CacheKey::new(path.clone(), path),
                meta,
                body: Bytes::from(vec![b'x'; body_len]),
                weight,
            }))
        })
        .collect()
}

async fn bounded(
    policy: Eviction,
    objects: &[Arc<Object>],
    requests: &[usize],
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let store = BoundedStore::new(&CacheDefaults {
        max_memory: ByteSize(BUDGET as u64),
        eviction: policy,
        ..Default::default()
    });
    let trace = Span::inactive().handle();
    let mut hits = 0;
    let mut peak = 0;
    for key in requests {
        let object = &objects[*key];
        if store.lookup(&object.key, &trace).await?.is_some() {
            hits += 1;
        } else {
            let mut writer = store
                .get_miss_handler(&object.key, &object.meta, &trace)
                .await?;
            writer.write_body(object.body.clone(), true).await?;
            writer.finish().await?;
        }
        peak = peak.max(store.bytes_used());
        assert!(peak <= BUDGET);
    }
    Ok((hits, peak))
}

fn tiny(objects: &[Arc<Object>], requests: &[usize]) -> (usize, usize) {
    let cache: TinyUfo<usize, (usize, Arc<Object>)> = TinyUfo::new(BUDGET, KEYS);
    let mut retained = HashMap::new();
    let mut hits = 0;
    let mut peak = 0;
    for key in requests {
        if cache.get(key).is_some() {
            hits += 1;
        } else {
            let object = objects[*key].clone();
            let weight = object.weight;
            // The returned data carries the original key; TinyUFO returns a
            // hashed key, which cannot directly clean Tanod's tag index.
            retained.insert(*key, usize::from(weight));
            for removed in cache.put(*key, (*key, object), weight) {
                retained.remove(&removed.data.0);
            }
        }
        let used: usize = retained.values().sum();
        peak = peak.max(used);
        assert!(used <= BUDGET);
    }
    (hits, peak)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let requests = workload();
    println!("workload,policy,trial,hits,requests,hit_ratio,peak_accounted_bytes");
    for (name, mixed) in [("equal_400B", false), ("mixed_400_to_4600B", true)] {
        let objects = objects(mixed)?;
        for (policy, label) in [
            (Eviction::Fifo, "fifo"),
            (Eviction::Clock, "clock"),
            (Eviction::Tinyufo, "tinyufo"),
        ] {
            let trials = if policy == Eviction::Tinyufo { 5 } else { 1 };
            for trial in 1..=trials {
                let (hits, peak) = bounded(policy, &objects, &requests).await?;
                println!(
                    "{name},{label},{trial},{hits},{REQUESTS},{:.4},{peak}",
                    hits as f64 / REQUESTS as f64
                );
            }
        }
        // TinyUFO seeds its internal hashing randomly. Report several trials
        // instead of presenting one randomized admission sketch as a constant.
        for trial in 1..=5 {
            let (hits, peak) = tiny(&objects, &requests);
            println!(
                "{name},tinyufo_reference,{trial},{hits},{REQUESTS},{:.4},{peak}",
                hits as f64 / REQUESTS as f64
            );
        }
    }
    Ok(())
}
