//! Keeps each upstream's address set current.
//!
//! An upstream is a DNS name that stands for a set of origin instances, and
//! that set changes: an instance is added, replaced after a crash, or moved by
//! a rollout. [`Resolver`] looks every name up again on `origin.resolve_interval`
//! so new instances start receiving traffic and replaced ones stop, without a
//! Tanod restart. Lookups are blocking (`getaddrinfo`), so they run on the
//! blocking pool, never on a proxy worker.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;

use super::UpstreamPool;

pub struct Resolver {
    pool: Arc<UpstreamPool>,
    interval: Duration,
}

impl Resolver {
    pub fn new(pool: Arc<UpstreamPool>, interval: Duration) -> Self {
        Resolver { pool, interval }
    }

    /// One pass over every backend. Returns how many changed their addresses.
    pub async fn refresh_all(&self) -> usize {
        let mut changed = 0;
        for backend in self.pool.backends() {
            let target = backend.clone();
            match tokio::task::spawn_blocking(move || target.refresh()).await {
                Ok(Ok(true)) => {
                    changed += 1;
                    let addresses: Vec<String> =
                        backend.sockets().iter().map(ToString::to_string).collect();
                    log::info!(
                        "upstream {} now resolves to {} address(es): {}",
                        backend.address,
                        addresses.len(),
                        addresses.join(", ")
                    );
                }
                Ok(Ok(false)) => {}
                // Kept serving on the previous addresses; see Backend::refresh.
                Ok(Err(error)) => log::warn!("{error}; keeping the previous addresses"),
                Err(error) => log::warn!(
                    "upstream {} lookup did not finish: {error}",
                    backend.address
                ),
            }
            crate::telemetry::metrics::UPSTREAM_ADDRESSES
                .with_label_values(&[&backend.address])
                .set(i64::try_from(backend.sockets().len()).unwrap_or(i64::MAX));
        }
        changed
    }
}

#[async_trait]
impl BackgroundService for Resolver {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(self.interval) => {}
            }
            self.refresh_all().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Breaker as BreakerConfig, LoadBalancing};

    #[tokio::test]
    async fn a_refresh_that_finds_the_same_addresses_changes_nothing() {
        let pool = Arc::new(
            UpstreamPool::new(
                &["127.0.0.1:3000".to_string()],
                LoadBalancing::RoundRobin,
                &BreakerConfig::default(),
            )
            .expect("pool"),
        );
        let resolver = Resolver::new(pool.clone(), Duration::from_secs(10));

        assert_eq!(resolver.refresh_all().await, 0);
        assert_eq!(pool.backends()[0].sockets().len(), 1);
    }
}
