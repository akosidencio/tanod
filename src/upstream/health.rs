//! Active health checking.
//!
//! Runs as a Pingora background service so it shares the server's runtime and
//! shutdown signal rather than needing a runtime of its own.
//!
//! A backend flips state only after a *streak*, not a single probe. One failed
//! probe during a GC pause should not drain a healthy backend, and one lucky
//! probe should not return a flapping one to rotation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::UpstreamPool;
use crate::config::schema::Health;

pub struct HealthChecker {
    pool: Arc<UpstreamPool>,
    path: String,
    interval: Duration,
    timeout: Duration,
    healthy_after: u32,
    unhealthy_after: u32,
    /// Consecutive successes and failures per backend.
    streak_ok: Vec<AtomicU32>,
    streak_fail: Vec<AtomicU32>,
}

impl HealthChecker {
    pub fn new(pool: Arc<UpstreamPool>, cfg: &Health) -> Self {
        let n = pool.len();
        HealthChecker {
            path: cfg.path.clone(),
            interval: cfg.interval.as_duration(),
            timeout: cfg.timeout.as_duration(),
            healthy_after: cfg.healthy_after.max(1),
            unhealthy_after: cfg.unhealthy_after.max(1),
            streak_ok: (0..n).map(|_| AtomicU32::new(0)).collect(),
            streak_fail: (0..n).map(|_| AtomicU32::new(0)).collect(),
            pool,
        }
    }

    /// Record a probe result and flip the backend if the streak is long enough.
    ///
    /// Returns `Some(new_state)` when the state actually changed.
    fn record(&self, id: usize, ok: bool) -> Option<bool> {
        let (ok_streak, fail_streak) = (&self.streak_ok[id], &self.streak_fail[id]);
        if ok {
            fail_streak.store(0, Ordering::Relaxed);
            let n = ok_streak.fetch_add(1, Ordering::Relaxed) + 1;
            if n == self.healthy_after {
                self.pool.set_healthy(id, true);
                return Some(true);
            }
        } else {
            ok_streak.store(0, Ordering::Relaxed);
            let n = fail_streak.fetch_add(1, Ordering::Relaxed) + 1;
            if n == self.unhealthy_after {
                self.pool.set_healthy(id, false);
                return Some(false);
            }
        }
        None
    }

    async fn probe(&self, address: &str) -> bool {
        let attempt = async {
            let mut sock = TcpStream::connect(address).await.ok()?;
            let host = address
                .rsplit_once(':')
                .map(|(host, _)| host.trim_matches(['[', ']']))
                .unwrap_or(address);
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: tanod-health\r\nConnection: close\r\n\r\n",
                self.path
            );
            sock.write_all(req.as_bytes()).await.ok()?;
            let mut status = Vec::with_capacity(128);
            let mut buf = [0u8; 128];
            loop {
                let n = sock.read(&mut buf).await.ok()?;
                if n == 0 {
                    return None;
                }
                status.extend_from_slice(&buf[..n]);
                if status.windows(2).any(|window| window == b"\r\n") {
                    break;
                }
                if status.len() >= 1024 {
                    return None;
                }
            }
            let line = String::from_utf8_lossy(&status);
            // Any 2xx counts. A backend that answers 204 to its own health
            // endpoint is healthy, and insisting on 200 would be pedantry.
            Some(line.starts_with("HTTP/1.1 2") || line.starts_with("HTTP/1.0 2"))
        };
        matches!(
            tokio::time::timeout(self.timeout, attempt).await,
            Ok(Some(true))
        )
    }
}

#[async_trait]
impl BackgroundService for HealthChecker {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        loop {
            // Probe immediately at startup. Strict readiness intentionally
            // begins unknown; making it wait one full interval before it can
            // become ready would add avoidable rollout latency.
            for backend in self.pool.backends() {
                let ok = self.probe(&backend.address).await;
                if let Some(state) = self.record(backend.id, ok) {
                    if state {
                        log::info!("upstream {} is healthy again", backend.address);
                    } else {
                        log::warn!("upstream {} marked unhealthy", backend.address);
                    }
                }
                // Republished every round rather than only on a flip. A gauge
                // that is only written on transitions reads as absent for the
                // whole first interval, and "no data" during an incident is
                // worse than a stale zero.
                crate::telemetry::metrics::UPSTREAM_HEALTHY
                    .with_label_values(&[&backend.address])
                    .set(i64::from(self.pool.is_healthy(backend.id)));
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(self.interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::LoadBalancing;
    use crate::config::units::Dur;

    fn checker(healthy_after: u32, unhealthy_after: u32) -> HealthChecker {
        let pool = Arc::new(
            UpstreamPool::new(
                &["127.0.0.1:1".to_string(), "127.0.0.2:1".to_string()],
                LoadBalancing::RoundRobin,
                &crate::config::schema::Breaker::default(),
            )
            .unwrap(),
        );
        HealthChecker::new(
            pool,
            &Health {
                path: "/healthz".into(),
                interval: Dur(Duration::from_secs(5)),
                timeout: Dur(Duration::from_secs(1)),
                healthy_after,
                unhealthy_after,
            },
        )
    }

    #[test]
    fn one_failed_probe_does_not_drain_a_backend() {
        let c = checker(2, 3);
        assert_eq!(c.record(0, false), None);
        assert_eq!(c.record(0, false), None);
        assert_eq!(
            c.record(0, false),
            Some(false),
            "flips on the third failure"
        );
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let c = checker(2, 3);
        c.record(0, false);
        c.record(0, false);
        c.record(0, true); // recovered before the threshold
        assert_eq!(c.record(0, false), None);
        assert_eq!(c.record(0, false), None);
        assert_eq!(c.record(0, false), Some(false));
    }

    #[test]
    fn recovery_also_requires_a_streak() {
        let c = checker(2, 1);
        assert_eq!(c.record(0, false), Some(false));
        assert_eq!(c.record(0, true), None, "one good probe is not enough");
        assert_eq!(c.record(0, true), Some(true));
    }

    #[test]
    fn backends_are_tracked_independently() {
        let c = checker(1, 1);
        assert_eq!(c.record(0, false), Some(false));
        assert_eq!(c.record(1, false), Some(false));
        assert_eq!(c.record(0, true), Some(true));
    }

    #[tokio::test]
    async fn a_probe_to_a_dead_address_fails_rather_than_hanging() {
        let c = checker(1, 1);
        // Port 1 on loopback refuses; the probe must resolve, not stall.
        assert!(!c.probe("127.0.0.1:1").await);
    }
}
