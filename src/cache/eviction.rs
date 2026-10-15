//! TinyUFO selects completed entries; the streaming store owns exact bytes.
//! All mutations run under the store's accounting and cache write locks.
use std::sync::Arc;
use tinyufo::TinyUfo;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Key {
    Entry { hash: Arc<str>, revision: u64 },
    Pressure(u64),
}

#[derive(Clone)]
struct Token {
    key: Key,
    weight: u16,
}

pub(super) struct Victim {
    pub hash: Arc<str>,
    pub revision: u64,
}

pub(super) struct TinyPolicy {
    index: TinyUfo<Key, Token>,
    unit: usize,
    capacity: usize,
    estimated: usize,
    weight: usize,
    revision: u64,
    pressure: u64,
    retired: usize,
}

impl TinyPolicy {
    pub fn new(bytes: usize) -> Self {
        // At most 2^24 weight units: pressure reservations need at most 257
        // u16 chunks. Small stores use exact byte weights; larger ones round
        // up conservatively. Exact byte accounting still belongs to the store.
        let unit = bytes.div_ceil(1 << 24).max(1);
        let capacity = bytes / unit;
        let estimated = (bytes / 1024).clamp(16, 65_536);
        Self {
            index: TinyUfo::new(capacity, estimated),
            unit,
            capacity,
            estimated,
            weight: 0,
            revision: 0,
            pressure: 0,
            retired: 0,
        }
    }

    fn units(&self, bytes: usize) -> u16 {
        // A very large entry can exceed u16 units. Saturation affects only
        // victim preference; the store enforces its complete, exact size.
        u16::try_from(bytes.div_ceil(self.unit).max(1)).unwrap_or(u16::MAX)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn invalidate(&mut self) {
        // New content gets a distinct queue identity. TinyUFO remove() leaves
        // lazy queue records; reusing their identity can target a replacement.
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn hit(&self, hash: &Arc<str>, revision: u64) {
        let _ = self.index.get(&Key::Entry {
            hash: hash.clone(),
            revision,
        });
    }

    pub fn remove(&mut self, hash: &str, revision: u64) {
        if let Some(token) = self.index.remove(&Key::Entry {
            hash: Arc::from(hash),
            revision,
        }) {
            self.weight = self.weight.saturating_sub(usize::from(token.weight));
            self.retired = self.retired.saturating_add(1);
        }
    }

    pub fn admit(&mut self, hash: &str, bytes: usize) -> Vec<Victim> {
        let key = Key::Entry {
            hash: Arc::from(hash),
            revision: self.revision,
        };
        let weight = self.units(bytes);
        self.weight += usize::from(weight);
        let removed = self.index.put(key.clone(), Token { key, weight }, weight);
        self.victims(removed)
    }

    fn victims(&mut self, removed: Vec<tinyufo::KV<Token>>) -> Vec<Victim> {
        removed
            .into_iter()
            .filter_map(|item| {
                if let Key::Entry { hash, revision } = item.data.key {
                    self.weight = self.weight.saturating_sub(usize::from(item.data.weight));
                    Some(Victim { hash, revision })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Ask TinyUFO for a victim without allocating an unaccounted response.
    /// Its public API evicts on insertion, so temporary pressure tokens fill
    /// the unused policy weight. They carry no body and are removed afterward.
    pub fn evict(&mut self) -> Vec<Victim> {
        if self.weight == 0 {
            return Vec::new();
        }
        let mut needed = self.capacity.saturating_sub(self.weight).saturating_add(1);
        let mut markers = Vec::new();
        let mut victims = Vec::new();
        while needed > 0 {
            let weight = u16::try_from(needed).unwrap_or(u16::MAX);
            needed -= usize::from(weight);
            let key = Key::Pressure(self.pressure);
            self.pressure = self.pressure.wrapping_add(1);
            let removed = self.index.force_put(
                key.clone(),
                Token {
                    key: key.clone(),
                    weight,
                },
                weight,
            );
            // Make younger pressure tokens at least as visited as an entry.
            // S3-FIFO then chooses a completed entry before a pressure token.
            for _ in 0..3 {
                let _ = self.index.get(&key);
            }
            markers.push(key);
            victims.extend(self.victims(removed));
        }
        for key in markers {
            if self.index.remove(&key).is_some() {
                self.retired = self.retired.saturating_add(1);
            }
        }
        victims
    }

    pub fn compact_if_needed<'a>(
        &mut self,
        entries: impl Iterator<Item = (&'a str, u64, usize, bool)>,
        count: usize,
    ) {
        // Bound lazy records even when a small working set is purged/refilled
        // forever without filling the byte budget. This amortizes rebuilding
        // over removals rather than rebuilding on every invalidation.
        if self.retired <= count.saturating_mul(2).saturating_add(64) {
            return;
        }
        self.index = TinyUfo::new(self.capacity, self.estimated);
        self.weight = 0;
        self.retired = 0;
        for (hash, revision, bytes, visited) in entries {
            let key = Key::Entry {
                hash: Arc::from(hash),
                revision,
            };
            let weight = self.units(bytes);
            self.weight += usize::from(weight);
            let _ = self.index.force_put(
                key.clone(),
                Token {
                    key: key.clone(),
                    weight,
                },
                weight,
            );
            if visited {
                for _ in 0..3 {
                    let _ = self.index.get(&key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_handles_large_weight_gaps_with_bounded_tokens() {
        let mut policy = TinyPolicy::new(1 << 30);
        assert!(policy.admit("large", 128 * 1024 * 1024).is_empty());
        let victims = policy.evict();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].hash.as_ref(), "large");
        assert_eq!(policy.weight, 0);
        assert!(policy.retired <= 257);
        assert!(policy.evict().is_empty());
        policy.compact_if_needed(std::iter::empty(), 0);
        assert_eq!(policy.retired, 0);
    }

    #[test]
    fn repeated_removal_compacts_lazy_queue_records() {
        let mut policy = TinyPolicy::new(64 * 1024);
        for _ in 0..2_000 {
            assert!(policy.admit("same", 1000).is_empty());
            policy.remove("same", policy.revision());
            policy.invalidate();
            policy.compact_if_needed(std::iter::empty(), 0);
            assert_eq!(policy.weight, 0);
            assert!(policy.retired <= 64);
        }
    }
}
