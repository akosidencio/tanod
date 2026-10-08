//! Drain state: the window between "stop sending me work" and "stop serving".
//!
//! # Why a window is needed at all
//!
//! A load balancer learns an instance is unhealthy by *polling* it. If the
//! process starts refusing connections the instant it is signalled, every
//! request the balancer sent in the last poll interval is lost — and that is
//! the ordinary case, not the unlucky one. Draining inverts the order:
//! readiness starts failing while the process is still perfectly able to
//! serve, so the balancer withdraws it on its own schedule, and only then does
//! the shutdown begin.
//!
//! Tanod enters this state from two places:
//!
//! * `SIGTERM` — [`DrainShutdownSignalWatch`] enters drain state and waits for
//!   the configured window *before* it returns the signal to Pingora. Pingora
//!   stops every listener as soon as it receives that signal, so doing this
//!   after its shutdown broadcast would make readiness and traffic disappear
//!   at the same instant.
//! * `SIGUSR1` — drain *without* exiting. This is the one a Kubernetes
//!   `preStop` hook or a rolling-restart script wants: send it, wait for the
//!   balancer to withdraw the instance, then send `SIGTERM`. Nothing else in
//!   the process reacts to `SIGUSR1`, and Pingora does not claim it.
//!
//! Draining never affects liveness. A draining instance is alive and serving;
//! an orchestrator that killed it here would be undoing the whole mechanism.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use pingora_core::server::{
    ShutdownSignal, ShutdownSignalWatch, ShutdownWatch, UnixShutdownSignalWatch,
};
use pingora_core::services::background::BackgroundService;
use tokio::signal::unix::{SignalKind, signal};

/// Shared, cheap to read from the request path and from the admin endpoints.
pub struct DrainState {
    draining: AtomicBool,
    /// When draining began, and why. Behind a lock because the pair has to be
    /// consistent; read only by the admin surface, never on the request path.
    since: Mutex<Option<(Instant, &'static str)>>,
}

impl Default for DrainState {
    fn default() -> Self {
        DrainState::new()
    }
}

impl DrainState {
    pub fn new() -> DrainState {
        DrainState {
            draining: AtomicBool::new(false),
            since: Mutex::new(None),
        }
    }

    /// Enter drain state. Idempotent: the *first* reason and instant are kept,
    /// so a `SIGUSR1` followed by a `SIGTERM` still reports how long the
    /// instance has actually been out of rotation.
    ///
    /// `reason` is `&'static str` rather than a `String` on purpose. It is
    /// rendered into the admin status document, and a closed set of literals
    /// chosen in this crate cannot carry anything a caller supplied.
    pub fn begin(&self, reason: &'static str) {
        let mut since = self.since.lock();
        if since.is_none() {
            *since = Some((Instant::now(), reason));
        }
        // Released after the timestamp is in place, so an observer that sees
        // `draining == true` always finds a `since` behind it.
        self.draining.store(true, Ordering::Release);
        crate::telemetry::metrics::DRAINING.set(1);
        log::warn!("draining: readiness now reports not-ready ({reason})");
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    pub fn draining_for(&self) -> Option<Duration> {
        self.since.lock().map(|(at, _)| at.elapsed())
    }

    pub fn reason(&self) -> &'static str {
        self.since.lock().map_or("-", |(_, why)| why)
    }
}

/// Turns `SIGUSR1` into drain state without terminating the process.
///
/// Automatic `SIGTERM` draining is deliberately handled by
/// [`DrainShutdownSignalWatch`] before Pingora sees the signal. This background
/// task only owns the manual pre-stop signal and exits when Pingora eventually
/// broadcasts shutdown.
pub struct DrainWatcher {
    state: Arc<DrainState>,
}

impl DrainWatcher {
    pub fn new(state: Arc<DrainState>) -> DrainWatcher {
        DrainWatcher { state }
    }
}

#[async_trait]
impl BackgroundService for DrainWatcher {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        // `SIGUSR1` is unclaimed: Pingora takes SIGQUIT, SIGTERM and SIGINT,
        // and Tanod's reloader takes SIGHUP.
        let mut usr1 = match signal(SignalKind::user_defined1()) {
            Ok(s) => Some(s),
            Err(error) => {
                // Not fatal. The automatic drain on shutdown still works; only
                // the manual pre-stop trigger is unavailable.
                log::error!(
                    "could not install the SIGUSR1 handler; `drain on demand` is unavailable: {error}"
                );
                None
            }
        };
        loop {
            let usr1_recv = async {
                match usr1.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    // No handler: never completes, so `select!` falls through
                    // to the shutdown arm.
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.changed() => {
                    return;
                }
                () = usr1_recv => {
                    // Drain without exiting: the pre-stop hook's signal.
                    self.state.begin("sigusr1");
                }
            }
        }
    }
}

/// Delays graceful termination until readiness has been failing for the full
/// load-balancer drain window.
///
/// Pingora broadcasts shutdown to its traffic, metrics and admin listeners at
/// once. Their accept loops stop on that broadcast, before Pingora sleeps its
/// own grace period. Intercepting `SIGTERM` here is therefore the only point at
/// which Tanod can remain reachable while advertising itself not-ready.
pub struct DrainShutdownSignalWatch {
    inner: UnixShutdownSignalWatch,
    state: Arc<DrainState>,
    drain_period: Duration,
    supervised_origin: bool,
}

impl DrainShutdownSignalWatch {
    pub fn new(state: Arc<DrainState>, drain_period: Duration) -> Self {
        Self {
            inner: UnixShutdownSignalWatch,
            state,
            drain_period,
            supervised_origin: false,
        }
    }

    /// This process supervises its origin (`origin.command`). A graceful
    /// upgrade hands the listeners to a new process, which would start a
    /// second origin on the same port while this one still runs, so `SIGQUIT`
    /// becomes a drain and stop instead.
    pub fn with_supervised_origin(mut self) -> Self {
        self.supervised_origin = true;
        self
    }

    async fn prepare(&self, signal: ShutdownSignal) -> ShutdownSignal {
        let signal = if self.supervised_origin && matches!(&signal, ShutdownSignal::GracefulUpgrade)
        {
            log::warn!(
                "graceful upgrade (SIGQUIT) cannot hand over a supervised origin; draining and \
                 stopping instead"
            );
            ShutdownSignal::GracefulTerminate
        } else {
            signal
        };
        if !matches!(&signal, ShutdownSignal::GracefulTerminate) {
            return signal;
        }

        self.state.begin("sigterm");
        let elapsed = self.state.draining_for().unwrap_or_default();
        let remaining = self.drain_period.saturating_sub(elapsed);
        if !remaining.is_zero() {
            log::info!(
                "draining for {:?} more before Pingora stops accepting connections",
                remaining
            );
            tokio::time::sleep(remaining).await;
        }
        signal
    }
}

#[async_trait]
impl ShutdownSignalWatch for DrainShutdownSignalWatch {
    async fn recv(&self) -> ShutdownSignal {
        self.prepare(self.inner.recv().await).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_state_is_not_draining() {
        let s = DrainState::new();
        assert!(!s.is_draining());
        assert!(s.draining_for().is_none());
        assert_eq!(s.reason(), "-");
    }

    #[test]
    fn beginning_a_drain_records_when_and_why() {
        let s = DrainState::new();
        s.begin("sigusr1");
        assert!(s.is_draining());
        assert_eq!(s.reason(), "sigusr1");
        assert!(s.draining_for().is_some());
    }

    #[test]
    fn a_second_drain_does_not_restart_the_clock() {
        // A pre-stop `SIGUSR1` followed by `SIGTERM` must still report how
        // long the instance has actually been out of rotation, or the
        // operator reading it concludes the drain window never elapsed.
        let s = DrainState::new();
        s.begin("sigusr1");
        std::thread::sleep(Duration::from_millis(20));
        s.begin("sigterm");
        assert_eq!(s.reason(), "sigusr1", "the first reason must be kept");
        assert!(
            s.draining_for().unwrap() >= Duration::from_millis(20),
            "the drain clock was reset"
        );
    }

    #[tokio::test]
    async fn the_manual_watcher_exits_when_the_server_broadcasts_shutdown() {
        let state = Arc::new(DrainState::new());
        let watcher = DrainWatcher::new(state.clone());
        let (tx, rx) = tokio::sync::watch::channel(false);

        let task = tokio::spawn(async move { watcher.start(rx).await });
        assert!(!state.is_draining());
        tx.send(true).unwrap();
        task.await.unwrap();

        assert!(
            !state.is_draining(),
            "the post-broadcast watcher drained too late"
        );
    }

    #[tokio::test]
    async fn graceful_termination_waits_before_pingora_sees_the_signal() {
        let state = Arc::new(DrainState::new());
        let watcher = DrainShutdownSignalWatch::new(state.clone(), Duration::from_millis(200));
        let started = Instant::now();
        let signal = watcher.prepare(ShutdownSignal::GracefulTerminate).await;

        assert!(matches!(signal, ShutdownSignal::GracefulTerminate));
        assert!(state.is_draining());
        assert_eq!(state.reason(), "sigterm");
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "the drain window was skipped: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn fast_shutdown_is_not_delayed_or_reported_as_a_drain() {
        let state = Arc::new(DrainState::new());
        let watcher = DrainShutdownSignalWatch::new(state.clone(), Duration::from_secs(30));
        let started = Instant::now();
        let signal = watcher.prepare(ShutdownSignal::FastShutdown).await;

        assert!(matches!(signal, ShutdownSignal::FastShutdown));
        assert!(!state.is_draining());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_manual_pre_drain_is_not_paid_twice_on_sigterm() {
        let state = Arc::new(DrainState::new());
        state.begin("sigusr1");
        tokio::time::sleep(Duration::from_millis(120)).await;
        let watcher = DrainShutdownSignalWatch::new(state, Duration::from_millis(100));
        let started = Instant::now();

        let signal = watcher.prepare(ShutdownSignal::GracefulTerminate).await;

        assert!(matches!(signal, ShutdownSignal::GracefulTerminate));
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
