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
use pingora_error::{Error, ErrorType};
use pingora_http::RequestHeader;
use pingora_load_balancing::health_check::{HealthCheck, HttpHealthCheck};
use pingora_load_balancing::selection::{SelectionAlgorithm, algorithms::RoundRobin};

use super::UpstreamPool;
use crate::config::schema::Health;

pub struct HealthChecker {
    pool: Arc<UpstreamPool>,
    probes: Vec<HttpHealthCheck>,
    probe_rotation: Vec<RoundRobin>,
    interval: Duration,
    timeout: Duration,
    healthy_after: u32,
    unhealthy_after: u32,
    /// Consecutive successes and failures per backend.
    streak_ok: Vec<AtomicU32>,
    streak_fail: Vec<AtomicU32>,
}

impl HealthChecker {
    pub fn new(pool: Arc<UpstreamPool>, cfg: &Health) -> Result<Self, String> {
        let n = pool.len();
        let probes = pool
            .backends()
            .iter()
            .map(|backend| {
                // Construct the configured request fallibly before using Pingora's
                // connector. Its constructor only sees a constant valid host.
                let mut req = RequestHeader::build("GET", cfg.path.as_bytes(), None)
                    .map_err(|e| format!("invalid health request: {e}"))?;
                let host = backend
                    .address
                    .rsplit_once(':')
                    .map(|(host, _)| host)
                    .unwrap_or(&backend.address);
                req.insert_header("Host", host).map_err(|e| e.to_string())?;
                req.insert_header("User-Agent", "tanod-health")
                    .map_err(|e| e.to_string())?;
                req.insert_header("Connection", "close")
                    .map_err(|e| e.to_string())?;
                let mut probe = HttpHealthCheck::new("localhost", false);
                probe.req = req;
                probe.peer_template.options.connection_timeout = Some(cfg.timeout.as_duration());
                probe.peer_template.options.read_timeout = Some(cfg.timeout.as_duration());
                probe.peer_template.options.write_timeout = Some(cfg.timeout.as_duration());
                probe.validator = Some(Box::new(|response| {
                    if response.status.is_success() {
                        Ok(())
                    } else {
                        Error::e_explain(
                            ErrorType::HTTPStatus(response.status.as_u16()),
                            "health endpoint returned a non-2xx response",
                        )
                    }
                }));
                Ok(probe)
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(HealthChecker {
            probes,
            probe_rotation: (0..n).map(|_| RoundRobin::new()).collect(),
            interval: cfg.interval.as_duration(),
            timeout: cfg.timeout.as_duration(),
            healthy_after: cfg.healthy_after.max(1),
            unhealthy_after: cfg.unhealthy_after.max(1),
            streak_ok: (0..n).map(|_| AtomicU32::new(0)).collect(),
            streak_fail: (0..n).map(|_| AtomicU32::new(0)).collect(),
            pool,
        })
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

    async fn probe(&self, backend: &super::Backend) -> bool {
        // Probe one current resolved instance per round, rotating through the
        // name's set. Health and streaks remain per configured upstream.
        let sockets = backend.sockets();
        let index =
            usize::try_from(self.probe_rotation[backend.id].next(b"") % sockets.len() as u64)
                .unwrap_or(0);
        let target = pingora_load_balancing::Backend {
            addr: pingora_core::protocols::l4::socket::SocketAddr::Inet(sockets[index]),
            weight: 1,
            ext: Default::default(),
        };
        // Per-I/O timeouts alone allow a trickling response to run forever.
        // Keep the configured deadline around the complete Pingora check.
        matches!(
            tokio::time::timeout(self.timeout, self.probes[backend.id].check(&target)).await,
            Ok(Ok(()))
        )
    }
}

#[async_trait]
impl BackgroundService for HealthChecker {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            // Probe immediately at startup. Strict readiness intentionally
            // begins unknown; making it wait one full interval before it can
            // become ready would add avoidable rollout latency.
            for backend in self.pool.backends() {
                let ok = tokio::select! {
                    _ = shutdown.changed() => return,
                    ok = self.probe(backend) => ok,
                };
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
        .unwrap()
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
        assert!(!c.probe(&c.pool.backends()[0]).await);
    }
    async fn probe_reply(response: &'static [u8], stall: bool) -> bool {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let pool = Arc::new(
            UpstreamPool::new(
                &[address],
                LoadBalancing::RoundRobin,
                &crate::config::schema::Breaker::default(),
            )
            .unwrap(),
        );
        let mut c = checker(1, 1);
        c.pool = pool;
        c.timeout = if stall {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /healthz HTTP/1.1\r\n"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("user-agent: tanod-health\r\n")
            );
            socket.write_all(response).await.unwrap();
            if stall {
                std::future::pending::<()>().await;
            }
        });
        let ok = c.probe(&c.pool.backends()[0]).await;
        if stall {
            server.abort();
        } else {
            server.await.unwrap();
        }
        ok
    }

    #[tokio::test]
    async fn pingora_accepts_200_with_a_body_and_204() {
        assert!(probe_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", false).await);
        assert!(probe_reply(b"HTTP/1.1 204 No Content\r\n\r\n", false).await);
    }

    #[tokio::test]
    async fn pingora_rejects_failures_redirects_and_malformed_status_lines() {
        assert!(
            !probe_reply(
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n",
                false
            )
            .await
        );
        assert!(!probe_reply(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n", false).await);
        assert!(!probe_reply(b"HTTP/1.1 2oops\r\nContent-Length: 0\r\n\r\n", false).await);
    }

    #[tokio::test]
    async fn the_deadline_includes_draining_the_response_body() {
        assert!(
            !probe_reply(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
                true
            )
            .await
        );
    }
    #[tokio::test]
    async fn shutdown_cancels_an_in_progress_health_check() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let pool = Arc::new(
            UpstreamPool::new(
                &[address],
                LoadBalancing::RoundRobin,
                &crate::config::schema::Breaker::default(),
            )
            .unwrap(),
        );
        let mut c = checker(1, 1);
        c.pool = pool;
        c.timeout = Duration::from_secs(5);
        let (signal, watch) = tokio::sync::watch::channel(false);
        let service = tokio::spawn(async move {
            c.start(watch).await;
        });
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        signal.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(200), service)
            .await
            .expect("shutdown should cancel the probe rather than wait five seconds")
            .unwrap();
    }
}
