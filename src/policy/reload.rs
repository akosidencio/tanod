//! Configuration reload on `SIGHUP`.
//!
//! Pingora already owns `SIGQUIT` (graceful upgrade), `SIGTERM` (graceful
//! terminate) and `SIGINT` (fast shutdown), so `SIGHUP` is the free one.
//!
//! Two rules make reload safe:
//!
//! 1. **A bad config is refused, and the running one is kept.** Reload happens
//!    when someone is changing something, which is often during an incident.
//!    Half-applying a config then is worse than not reloading at all.
//! 2. **Policy is swapped; stateful runtime is not.** Limiters are resized in
//!    place because their outstanding permits belong to them — swapping in a
//!    fresh semaphore would transiently double admitted concurrency, on
//!    precisely the change most likely to be made under load (raising a limit).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use tokio::signal::unix::{SignalKind, signal};

use super::PolicySnapshot;
use crate::admission::AdmissionController;

pub struct Reloader {
    path: String,
    policy: Arc<ArcSwap<PolicySnapshot>>,
    admission: Arc<AdmissionController>,
    /// The response cache, so a deployment rollover can reclaim the previous
    /// build's entries. `None` in tests that only exercise policy.
    store: Option<&'static crate::cache::BoundedStore>,
    generation: AtomicU64,
}

impl Reloader {
    pub fn new(
        path: impl Into<String>,
        policy: Arc<ArcSwap<PolicySnapshot>>,
        admission: Arc<AdmissionController>,
    ) -> Self {
        Reloader {
            path: path.into(),
            policy,
            admission,
            store: None,
            generation: AtomicU64::new(1),
        }
    }

    /// Give the reloader the cache, so that changing `deployment.id` reclaims
    /// the old build's entries instead of leaving them to age out.
    #[must_use]
    pub fn with_store(mut self, store: &'static crate::cache::BoundedStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Re-read, validate, and apply. Returns the new generation, or an
    /// explanation of why the running config was kept.
    pub fn reload(&self) -> Result<u64, String> {
        let cfg = crate::config::load(&self.path).map_err(|e| {
            let mut msg = e.to_string();
            if let Some(src) = std::error::Error::source(&e) {
                msg.push_str(&format!(": {src}"));
            }
            msg
        })?;

        let current = self.policy.load();

        // Listener and upstream topology cannot change under a running
        // server. Pingora's SIGQUIT graceful upgrade is the tool for that,
        // and silently ignoring the edit would be worse than refusing it.
        if cfg.server.listen != current.config.server.listen {
            return Err(format!(
                "server.listen changed from {} to {}; that needs a restart \
                 (SIGQUIT performs a graceful upgrade)",
                current.config.server.listen, cfg.server.listen
            ));
        }
        if cfg.origin.upstreams != current.config.origin.upstreams {
            return Err(
                "origin.upstreams changed; that needs a restart (SIGQUIT performs a \
                 graceful upgrade)"
                    .to_string(),
            );
        }
        if cfg.origin.load_balancing != current.config.origin.load_balancing {
            return Err("origin.load_balancing changed; that needs a restart".to_string());
        }
        if cfg.health != current.config.health {
            return Err("health-check settings changed; that needs a restart".to_string());
        }
        if cfg.telemetry.prometheus != current.config.telemetry.prometheus {
            return Err("telemetry.prometheus changed; that needs a restart".to_string());
        }
        if cfg.cache.max_memory != current.config.cache.max_memory
            || cfg.cache.store != current.config.cache.store
            || cfg.cache.eviction != current.config.cache.eviction
            || cfg.cache.tag_header != current.config.cache.tag_header
        {
            return Err(
                "cache store, memory budget, eviction policy or tag header changed; the store is \
                 built once at startup and its entries are already indexed under the old header, \
                 so that needs a restart"
                    .to_string(),
            );
        }
        if cfg.cache.purge.token != current.config.cache.purge.token {
            return Err(
                "cache.purge.token changed; the admin listener reads it once at startup, so a \
                 reload would report success while the old secret stayed in force; that needs a \
                 restart"
                    .to_string(),
            );
        }
        if cfg.timeouts.origin != current.config.timeouts.origin {
            return Err(
                "timeouts.origin changed; Pingora's cache-lock writer age is startup-bound and needs \
                 a restart"
                    .to_string(),
            );
        }
        // Everything below is compiled or bound once in `Tanod::new` and is
        // not swapped per request. Refusing the edit is the whole point: a
        // reload that reported success while leaving the old trust policy in
        // force would be a security setting that silently did not apply, and
        // an operator would have every reason to believe it had.
        if cfg.server.trusted_proxies != current.config.server.trusted_proxies {
            return Err(
                "server.trusted_proxies changed; the trust policy is compiled once at \
                 startup, so a reload would report success while the old one stayed in \
                 force; that needs a restart (SIGQUIT performs a graceful upgrade)"
                    .to_string(),
            );
        }
        if cfg.server.h2c != current.config.server.h2c
            || cfg.server.tls != current.config.server.tls
        {
            return Err(
                "server.h2c or server.tls changed; listeners are bound at startup and that needs a \
                 restart (SIGQUIT performs a graceful upgrade)"
                    .to_string(),
            );
        }
        if cfg.origin.tls != current.config.origin.tls
            || cfg.origin.http_version != current.config.origin.http_version
        {
            return Err(
                "origin.tls or origin.http_version changed; that needs a restart".to_string(),
            );
        }
        if cfg.origin.breaker != current.config.origin.breaker {
            return Err(
                "origin.breaker changed; each backend's failure window and its open/closed state                  are built once at startup, so a reload would report success while the old                  thresholds stayed in force; that needs a restart"
                    .to_string(),
            );
        }
        if cfg.origin.retry != current.config.origin.retry {
            return Err(
                "origin.retry changed; the retry budget's window is bound once at startup, and                  swapping it while retries are in flight would leave two windows disagreeing                  about what has been spent; that needs a restart"
                    .to_string(),
            );
        }
        if cfg.spool.max_memory != current.config.spool.max_memory {
            return Err(
                "spool.max_memory changed; the spool budget is allocated once at startup and that \
                 needs a restart. spool.enabled and spool.max_body do reload"
                    .to_string(),
            );
        }
        if cfg.server.graceful != current.config.server.graceful {
            return Err(
                "server.graceful changed; the pid file, upgrade socket and shutdown windows are \
                 read once when the process starts and are what a graceful upgrade coordinates \
                 on, so a reload would report success while the old paths stayed in force; that \
                 needs a restart"
                    .to_string(),
            );
        }
        if cfg.telemetry.admin != current.config.telemetry.admin {
            return Err(
                "telemetry.admin changed; the admin listener is bound at startup and that needs \
                 a restart"
                    .to_string(),
            );
        }
        if cfg.telemetry.tracing.otlp != current.config.telemetry.tracing.otlp
            || cfg.telemetry.tracing.service_name != current.config.telemetry.tracing.service_name
        {
            // Sampling and inbound trust *do* reload — they are read per
            // request from the snapshot. The exporter is not: its queue,
            // endpoint and resource attributes are bound once.
            return Err(
                "telemetry.tracing.otlp or service_name changed; the span exporter is built once \
                 at startup and that needs a restart. telemetry.tracing.sample and \
                 trust_incoming do reload"
                    .to_string(),
            );
        }
        if cfg.upgrade.max_concurrent != current.config.upgrade.max_concurrent {
            return Err(
                "upgrade.max_concurrent changed; the upgrade limiter is sized once at startup and \
                 that needs a restart"
                    .to_string(),
            );
        }
        let deployment_changed = cfg.deployment.id != current.config.deployment.id;
        drop(current);

        let route_limits: Vec<(String, usize, usize, std::time::Duration)> = cfg
            .routes
            .iter()
            .filter_map(|r| {
                r.concurrency.as_ref().map(|c| {
                    (
                        r.id.clone(),
                        c.max,
                        c.queue.max,
                        c.queue.timeout.as_duration(),
                    )
                })
            })
            .collect();

        let next_generation = self.generation.load(Ordering::SeqCst) + 1;
        let snapshot = PolicySnapshot::build(cfg, next_generation).map_err(|e| e.to_string())?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let fingerprint = snapshot.fingerprint;

        // Resize before swapping: a request arriving on the new policy should
        // never find the old ceiling still in force.
        let global = &snapshot.config.origin.concurrency;
        self.admission.apply_limits(
            global.max,
            global.queue.max,
            global.queue.timeout.as_duration(),
            &snapshot.config.origin.priorities,
            &route_limits,
        );
        for tier in self.admission.tier_limiters() {
            crate::telemetry::metrics::LIMIT
                .with_label_values(&[tier.name()])
                .set(i64::try_from(tier.limit()).unwrap_or(i64::MAX));
            crate::telemetry::metrics::QUEUE_DEPTH
                .with_label_values(&[tier.name()])
                .set(i64::try_from(tier.queue_depth()).unwrap_or(i64::MAX));
            crate::telemetry::metrics::IN_FLIGHT
                .with_label_values(&[tier.name()])
                .set(
                    i64::try_from(tier.limit().saturating_sub(tier.available()))
                        .unwrap_or(i64::MAX),
                );
        }
        // Deployment-safe invalidation. The cache key already carries
        // `deployment.id`, so the previous build's entries become unreachable
        // the instant the id changes — but unreachable is not reclaimed, and a
        // cache still holding a whole dead build has that much less room for
        // the one now serving traffic. Done before the swap, so no request on
        // the new generation observes the old entries occupying the budget.
        if deployment_changed && let Some(store) = self.store {
            let purged = store.purge_all();
            if purged.entries > 0 {
                log::info!(
                    "deployment id changed; purged {} cache entries ({} bytes) from the                      previous build",
                    purged.entries,
                    purged.bytes
                );
                crate::telemetry::metrics::CACHE_PURGED
                    .with_label_values(&["deployment"])
                    .inc_by(purged.entries as u64);
            }
        }
        self.policy.store(snapshot);
        // Published only after the swap succeeded, so the gauge answers "which
        // config is actually serving" rather than "which one was attempted".
        // A refused reload leaves it untouched, which is what makes it usable
        // as the check after a deploy.
        crate::telemetry::metrics::CONFIG_GENERATION
            .set(i64::try_from(generation).unwrap_or(i64::MAX));
        crate::telemetry::metrics::CONFIG_FINGERPRINT
            .set(i64::try_from(fingerprint).unwrap_or(i64::MAX));

        Ok(generation)
    }
}

#[async_trait]
impl BackgroundService for Reloader {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            log::error!("could not install the SIGHUP handler; reload is unavailable");
            return;
        };
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = hup.recv() => match self.reload() {
                    Ok(generation) => {
                        log::info!("config reloaded from {} (generation {generation})", self.path);
                    }
                    Err(why) => {
                        // Keep serving the config that was already working.
                        log::error!("reload refused, keeping the running config: {why}");
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> tempfile_lite::TempPath {
        let path = tempfile_lite::TempPath::new("tanod-reload");
        let mut f = std::fs::File::create(&path.0).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    const BASE: &str = r#"
version: 1
server:
  listen: "127.0.0.1:8080"
origin:
  upstreams: ["a:3000"]
  concurrency:
    max: 100
routes:
  - id: products
    match: "/products/**"
    concurrency:
      max: 10
"#;

    fn setup(
        body: &str,
    ) -> (
        tempfile_lite::TempPath,
        Reloader,
        Arc<ArcSwap<PolicySnapshot>>,
    ) {
        let path = write_config(body);
        let cfg = crate::config::load(&path.0).unwrap();
        let snapshot = PolicySnapshot::build(cfg, 1).unwrap();
        let admission = Arc::new(AdmissionController::new(
            100,
            0,
            std::time::Duration::ZERO,
            &crate::config::schema::Priorities::default(),
        ));
        let policy = Arc::new(ArcSwap::from(snapshot));
        let reloader = Reloader::new(path.0.clone(), policy.clone(), admission);
        (path, reloader, policy)
    }

    #[test]
    fn a_valid_change_is_applied_and_bumps_the_generation() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(&path.0, BASE.replace("max: 10", "max: 42")).unwrap();

        let generation = reloader.reload().unwrap();
        assert_eq!(generation, 2);
        assert_eq!(policy.load().generation, 2);
        let route = &policy.load().routes[0];
        assert_eq!(route.config.concurrency.as_ref().unwrap().max, 42);
    }

    #[test]
    fn an_invalid_config_is_refused_and_the_running_one_kept() {
        let (path, reloader, policy) = setup(BASE);
        // private_dynamic plus a cache override is a leak; validation refuses it.
        std::fs::write(
            &path.0,
            format!("{BASE}    class: private_dynamic\n    cache:\n      override_origin: true\n      ttl:\n        max: 2s\n"),
        )
        .unwrap();

        assert!(reloader.reload().is_err());
        assert_eq!(
            policy.load().generation,
            1,
            "a refused reload must not advance"
        );
        assert_eq!(
            policy.load().routes[0]
                .config
                .concurrency
                .as_ref()
                .unwrap()
                .max,
            10,
            "the running config must be untouched"
        );
    }

    #[test]
    fn unparseable_yaml_is_refused() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(&path.0, "version: 1\n  this: is not: valid yaml\n").unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(policy.load().generation, 1);
    }

    #[test]
    fn changing_the_listener_is_refused_with_an_explanation() {
        let (path, reloader, _policy) = setup(BASE);
        std::fs::write(&path.0, BASE.replace("127.0.0.1:8080", "127.0.0.1:9999")).unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(err.contains("needs a restart"), "{err}");
    }

    /// Priority shares are the one resilience knob an operator is likely to
    /// reach for mid-incident, so unlike the breaker and the retry budget they
    /// reload in place.
    #[test]
    fn changing_priority_shares_reloads_in_place() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(
            &path.0,
            BASE.replace(
                "  concurrency:\n    max: 100",
                "  concurrency:\n    max: 100\n  priorities:\n    low: 40",
            ),
        )
        .unwrap();
        reloader.reload().unwrap();
        assert_eq!(policy.load().config.origin.priorities.low, 40);
    }

    /// Put one entry in a store, so a purge has something to reclaim.
    async fn fill_one(store: &'static crate::cache::BoundedStore) {
        use pingora_cache::key::CacheKey;
        use pingora_cache::trace::Span;
        use pingora_cache::{CacheMeta, storage::Storage};
        use pingora_http::ResponseHeader;

        let now = std::time::SystemTime::now();
        let meta = CacheMeta::new(
            now + std::time::Duration::from_secs(600),
            now,
            0,
            0,
            ResponseHeader::build(200, None).unwrap(),
        );
        let mut writer = store
            .get_miss_handler(
                &CacheKey::new("", "/page", ""),
                &meta,
                &Span::inactive().handle(),
            )
            .await
            .unwrap();
        writer
            .write_body(bytes::Bytes::from_static(b"body"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();
    }

    /// A deployment rollover has to reclaim the previous build's entries.
    /// They are already unreachable — the id is in the cache key — but
    /// unreachable is not the same as returned to the byte budget, and a cache
    /// still holding a whole dead build has that much less room for the one
    /// now serving traffic.
    #[tokio::test]
    async fn changing_the_deployment_id_purges_the_previous_build() {
        let cache =
            crate::cache::BoundedStore::new(&crate::config::schema::CacheDefaults::default());
        let (path, reloader, policy) = setup(BASE);
        let reloader = reloader.with_store(cache);

        fill_one(cache).await;
        assert_eq!(cache.entries(), 1);
        assert!(cache.bytes_used() > 0);

        std::fs::write(&path.0, format!("{BASE}deployment:\n  id: \"build-2\"\n")).unwrap();
        reloader.reload().unwrap();

        assert_eq!(
            policy.load().config.deployment.id.as_deref(),
            Some("build-2")
        );
        assert_eq!(cache.entries(), 0, "the previous build's entries survived");
        assert_eq!(cache.bytes_used(), 0, "their bytes were never reclaimed");
    }

    /// A reload that leaves the deployment id alone must not empty the cache.
    /// Purging on every `SIGHUP` would turn a routine config change into an
    /// origin stampede — the exact failure this feature exists to avoid.
    #[tokio::test]
    async fn an_ordinary_reload_leaves_the_cache_alone() {
        let cache =
            crate::cache::BoundedStore::new(&crate::config::schema::CacheDefaults::default());
        let (path, reloader, _policy) = setup(BASE);
        let reloader = reloader.with_store(cache);

        fill_one(cache).await;
        std::fs::write(&path.0, BASE.replace("max: 100", "max: 120")).unwrap();
        reloader.reload().unwrap();
        assert_eq!(cache.entries(), 1, "a routine reload emptied the cache");
    }

    #[test]
    fn changing_upstreams_is_refused_with_an_explanation() {
        let (path, reloader, _policy) = setup(BASE);
        std::fs::write(
            &path.0,
            BASE.replace(r#"["a:3000"]"#, r#"["a:3000", "b:3000"]"#),
        )
        .unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(err.contains("needs a restart"), "{err}");
    }

    #[test]
    fn changing_startup_bound_runtime_components_is_refused() {
        for changed in [
            BASE.replace(
                "upstreams: [\"a:3000\"]",
                "upstreams: [\"a:3000\"]\n  load_balancing: hash_by_path",
            ),
            format!("{BASE}health:\n  path: /healthz\n  interval: 5s\n  timeout: 1s\n"),
            format!("{BASE}cache:\n  max_memory: 64MiB\n"),
            format!("{BASE}telemetry:\n  prometheus:\n    listen: \"127.0.0.1:9090\"\n"),
            // The security-relevant one. A reload that reported success while
            // leaving the old trust policy in force is a setting that silently
            // did not apply, and an operator would have every reason to
            // believe it had.
            BASE.replace(
                "  listen: \"127.0.0.1:8080\"",
                "  listen: \"127.0.0.1:8080\"\n  trusted_proxies:\n    from: [\"10.0.0.0/8\"]",
            ),
            BASE.replace(
                "  listen: \"127.0.0.1:8080\"",
                "  listen: \"127.0.0.1:8080\"\n  h2c: true",
            ),
            BASE.replace(
                "upstreams: [\"a:3000\"]",
                "upstreams: [\"a:3000\"]\n  http_version: http2",
            ),
            format!("{BASE}spool:\n  max_memory: 16MiB\n"),
            // The store is built once and its entries are already indexed
            // under the old tag header; the policy is baked into the store.
            format!("{BASE}cache:\n  eviction: fifo\n"),
            format!("{BASE}cache:\n  tag_header: \"x-next-cache-tags\"\n"),
            format!("{BASE}upgrade:\n  max_concurrent: 7\n"),
            // Each backend's failure window and its open/closed state are
            // built with the pool. A reload cannot rebuild them without
            // discarding everything observed so far.
            BASE.replace(
                "upstreams: [\"a:3000\"]",
                "upstreams: [\"a:3000\"]\n  breaker:\n    window: 20s",
            ),
            // The retry budget's window is bound once; two windows
            // disagreeing about what has been spent is not a budget.
            BASE.replace(
                "upstreams: [\"a:3000\"]",
                "upstreams: [\"a:3000\"]\n  retry:\n    window: 20s",
            ),
            // The upgrade socket is what two processes coordinate a
            // zero-downtime handoff on. A reload that reported success while
            // the old path stayed in force would break the next restart.
            BASE.replace(
                "  listen: \"127.0.0.1:8080\"",
                "  listen: \"127.0.0.1:8080\"\n  graceful:\n    upgrade_socket: /tmp/other.sock",
            ),
            format!("{BASE}telemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n"),
            format!(
                "{BASE}telemetry:\n  tracing:\n    otlp:\n      endpoint: \"http://127.0.0.1:4318/v1/traces\"\n"
            ),
        ] {
            let (path, reloader, policy) = setup(BASE);
            std::fs::write(&path.0, &changed).unwrap();
            let err = reloader.reload().unwrap_err();
            assert!(err.contains("restart"), "config:\n{changed}\nerror: {err}");
            assert_eq!(policy.load().generation, 1);
        }
    }

    #[test]
    fn sampling_and_trace_trust_do_reload() {
        // The counterpart to the refusals above. These are read per request
        // from the snapshot, so refusing them would be the opposite failure:
        // an operator unable to turn sampling down during an incident.
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(
            &path.0,
            format!(
                "{BASE}telemetry:\n  tracing:\n    sample:\n      mode: ratio\n      one_in: 50\n"
            ),
        )
        .unwrap();
        assert_eq!(reloader.reload().unwrap(), 2);
        assert_eq!(policy.load().config.telemetry.tracing.sample.one_in, 50);
    }

    #[test]
    fn an_edit_to_a_custom_overload_page_is_picked_up_on_reload() {
        let page = tempfile_lite::TempPath::new("tanod-page");
        std::fs::write(&page.0, "<p>{{title}}</p>").unwrap();
        let body = format!(
            "{BASE}overload:\n  page:\n    title: Sale\n    file: \"{}\"\n",
            page.0
        );
        let (_path, reloader, policy) = setup(&body);
        let html = |p: &Arc<ArcSwap<PolicySnapshot>>| p.load().overload_page.clone().unwrap().html;
        assert_eq!(&html(&policy)[..], b"<p>Sale</p>");

        std::fs::write(&page.0, "<b>{{title}}</b>").unwrap();
        reloader.reload().unwrap();
        assert_eq!(&html(&policy)[..], b"<b>Sale</b>");

        // A page file that disappears is a refused reload, not a blank page.
        std::fs::remove_file(&page.0).unwrap();
        let error = reloader.reload().unwrap_err();
        assert!(error.contains(&page.0), "{error}");
        assert_eq!(&html(&policy)[..], b"<b>Sale</b>");
    }

    #[test]
    fn a_missing_file_is_refused() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::remove_file(&path.0).unwrap();
        assert!(reloader.reload().is_err());
        assert_eq!(policy.load().generation, 1);
    }
}

/// Minimal temp-file helper. A whole crate for this in the dependency graph of
/// a proxy is not worth it.
#[cfg(test)]
mod tempfile_lite {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub struct TempPath(pub String);

    impl TempPath {
        pub fn new(prefix: &str) -> TempPath {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            TempPath(format!(
                "{}/{prefix}-{pid}-{n}.yaml",
                std::env::temp_dir().display()
            ))
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
