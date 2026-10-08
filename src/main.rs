//! `tanod` — origin workload governor.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
tanod — origin workload governor for server-rendered applications

USAGE:
    tanod init [OPTIONS]          Create a safe single-server config
    tanod check [--config <FILE>] Validate a config file and exit
    tanod version                 Print the version and build features

    tanod run [--config <FILE>] [OPTIONS]

CONFIG:
    --config, -c <FILE>  Use this file. Otherwise Tanod checks
                         $TANOD_CONFIG, ./tanod.yaml, ./tanod.yml,
                         /etc/tanod/tanod.yaml, then /etc/tanod/tanod.yml.

INIT OPTIONS:
    --upstream <ADDR>       Origin address (default: 127.0.0.1:3000)
    --listen <ADDR>         Tanod address (default: 127.0.0.1:8080)
    --concurrency <NUMBER>  Origin work ceiling (default: 16)
    --force                 Replace an existing config file

RUN OPTIONS:
    --upgrade    Take the listening sockets over from a running Tanod.
                 The old process keeps serving what it already accepted and
                 exits when those finish; no connection is refused in between.
                 Both processes must agree on server.graceful.upgrade_socket.
    --daemon     Fork into the background and write server.graceful.pid_file.
    --test       Bind everything, prove the process can start, and exit 0.
                 Run this before --upgrade: it turns \"the new binary cannot
                 start\" from an outage into a non-zero exit code.

SIGNALS:
    SIGHUP     reload the config; a bad one is refused and the running one kept
    SIGUSR1    start draining — readiness fails, traffic keeps being served
    SIGQUIT    graceful upgrade: hand the listeners to a process started with --upgrade
    SIGTERM    graceful shutdown
    SIGINT     fast shutdown
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        Some("version") => {
            // Features are part of the version because a binary that rejects
            // `server.tls` and one that terminates it are the same version
            // number otherwise, and "which build is this" is the first
            // question during an incident.
            println!(
                "tanod {} (config schema v{}, features: {})",
                env!("CARGO_PKG_VERSION"),
                tanod::config::SCHEMA_VERSION,
                if cfg!(feature = "tls") { "tls" } else { "none" }
            );
            ExitCode::SUCCESS
        }
        Some("init") if has_flag(&args, "--help") || has_flag(&args, "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("init") => init(&args),
        Some("check") => match resolve_config_path(&args) {
            Ok(path) => check(path.to_string_lossy().as_ref()),
            Err(error) => {
                eprintln!("tanod check: {error}");
                ExitCode::from(2)
            }
        },
        Some("run") => match resolve_config_path(&args) {
            Ok(path) => {
                let flags = RunFlags {
                    upgrade: has_flag(&args, "--upgrade"),
                    daemon: has_flag(&args, "--daemon"),
                    test: has_flag(&args, "--test"),
                };
                if let Some(unknown) = unknown_run_flag(&args) {
                    eprintln!("tanod run: unknown option `{unknown}`\n\n{USAGE}");
                    return ExitCode::from(2);
                }
                run(path.to_string_lossy().as_ref(), flags)
            }
            Err(error) => {
                eprintln!("tanod run: {error}");
                ExitCode::from(2)
            }
        },
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("tanod: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn resolve_config_path(args: &[String]) -> Result<PathBuf, String> {
    if let Some(i) = args.iter().position(|a| a == "--config" || a == "-c") {
        return args
            .get(i + 1)
            .filter(|value| !value.starts_with('-'))
            .map(PathBuf::from)
            .ok_or_else(|| "--config requires a file path".to_string());
    }

    if let Some(path) = std::env::var_os("TANOD_CONFIG").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    const DEFAULTS: [&str; 4] = [
        "tanod.yaml",
        "tanod.yml",
        "/etc/tanod/tanod.yaml",
        "/etc/tanod/tanod.yml",
    ];
    DEFAULTS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| {
            "no config found; run `tanod init`, set TANOD_CONFIG, or pass --config <FILE>"
                .to_string()
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitOptions {
    config: PathBuf,
    upstream: String,
    listen: String,
    concurrency: usize,
    force: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            config: PathBuf::from("tanod.yaml"),
            upstream: "127.0.0.1:3000".to_string(),
            listen: "127.0.0.1:8080".to_string(),
            concurrency: 16,
            force: false,
        }
    }
}

fn init(args: &[String]) -> ExitCode {
    let options = match parse_init_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("tanod init: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let contents = render_initial_config(&options);

    // Parse and validate the generated text before touching the destination.
    // This also validates user-provided listener and upstream addresses.
    let config: tanod::config::schema::Config = match serde_saphyr::from_str(&contents) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("tanod init: generated an invalid config: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = tanod::config::validation::validate(&config) {
        eprintln!("tanod init: {error}");
        return ExitCode::from(2);
    }
    if let Err(error) = tanod::policy::PolicySnapshot::build(config, 1) {
        eprintln!("tanod init: {error}");
        return ExitCode::from(2);
    }

    if options.config.exists() && !options.force {
        eprintln!(
            "tanod init: {} already exists; pass --force to replace it",
            options.config.display()
        );
        return ExitCode::from(2);
    }
    if let Some(parent) = options
        .config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("tanod init: could not create {}: {error}", parent.display());
        return ExitCode::FAILURE;
    }
    if let Err(error) = std::fs::write(&options.config, contents) {
        eprintln!(
            "tanod init: could not write {}: {error}",
            options.config.display()
        );
        return ExitCode::FAILURE;
    }

    println!("created {}", options.config.display());
    println!("  listen: {}", options.listen);
    println!("  upstream: {}", options.upstream);
    println!("  origin concurrency ceiling: {}", options.concurrency);
    println!("next: tanod check --config {}", options.config.display());
    ExitCode::SUCCESS
}

fn parse_init_options(args: &[String]) -> Result<InitOptions, String> {
    let mut options = InitOptions::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                options.config = PathBuf::from(option_value(args, &mut i, "--config")?);
            }
            "--upstream" => {
                options.upstream = option_value(args, &mut i, "--upstream")?.to_string();
            }
            "--listen" => {
                options.listen = option_value(args, &mut i, "--listen")?.to_string();
            }
            "--concurrency" => {
                let value = option_value(args, &mut i, "--concurrency")?;
                options.concurrency = value
                    .parse()
                    .map_err(|_| "--concurrency must be a positive integer".to_string())?;
                if options.concurrency == 0 {
                    return Err("--concurrency must be greater than zero".to_string());
                }
            }
            "--force" => options.force = true,
            unknown => return Err(format!("unknown option `{unknown}`")),
        }
        i += 1;
    }
    Ok(options)
}

fn option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    option: &str,
) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| format!("{option} requires a value"))
}

fn yaml_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn render_initial_config(options: &InitOptions) -> String {
    let queue = options.concurrency.saturating_mul(2);
    format!(
        r#"# Generated by `tanod init`. Add reviewed public routes above the private catch-all.
version: 1

server:
  listen: {}
  # The generated setup expects a local TLS edge such as Caddy.
  trusted_proxies:
    from: ["127.0.0.1/32", "::1/128"]
    client_ip: x_forwarded
    scheme: x_forwarded

origin:
  upstreams: [{}]
  concurrency:
    # Safe capacity depends on the application. Start here, measure, then tune.
    max: {}
    queue:
      max: {}
      timeout: 2s

telemetry:
  admin:
    listen: "127.0.0.1:9091"

routes:
  - id: default-private
    match: "/**"
    class: private_dynamic
    cache:
      enabled: false
    coalesce:
      enabled: false
"#,
        yaml_quote(&options.listen),
        yaml_quote(&options.upstream),
        options.concurrency,
        queue,
    )
}

/// What `run` was asked to do beyond starting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RunFlags {
    upgrade: bool,
    daemon: bool,
    test: bool,
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// An unrecognised `--flag`, if there is one.
///
/// The same rule the config file follows: an option that is accepted and then
/// ignored lets someone believe a process is daemonised, or upgrading, when it
/// is not. A typo has to be an error, not a silent default.
fn unknown_run_flag(args: &[String]) -> Option<&str> {
    let known = ["--upgrade", "--daemon", "--test", "--config", "-c", "run"];
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--config" || arg == "-c" {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') && !known.contains(&arg.as_str()) {
            return Some(arg);
        }
    }
    None
}

/// Pingora's limit counts total upstream tries, the same unit exposed by
/// `origin.retry.max_attempts`. Bind the framework ceiling to the policy rather
/// than leaving Pingora's independent default of 16 in force.
fn proxy_max_attempts(retry: &tanod::config::schema::Retry) -> usize {
    if retry.enabled {
        usize::try_from(retry.max_attempts).unwrap_or(usize::MAX)
    } else {
        1
    }
}

fn run(path: &str, flags: RunFlags) -> ExitCode {
    use std::sync::Arc;
    use tanod::admin::Admin;
    use tanod::admin::drain::{DrainShutdownSignalWatch, DrainState, DrainWatcher};
    use tanod::admission::AdmissionController;
    use tanod::policy::PolicySnapshot;
    use tanod::policy::reload::Reloader;
    use tanod::proxy::Tanod;
    use tanod::upstream::UpstreamPool;
    use tanod::upstream::health::HealthChecker;

    let cfg = match tanod::config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            if let Some(s) = std::error::Error::source(&e) {
                eprintln!("  caused by: {s}");
            }
            return ExitCode::FAILURE;
        }
    };

    // Logs go to stderr at info by default; RUST_LOG overrides as usual.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    tanod::telemetry::metrics::preregister();

    let listen = cfg.server.listen.clone();
    let origin_command = cfg.origin.command.clone();
    let origin_upstream = cfg.origin.upstreams.first().cloned().unwrap_or_default();
    let h2c = cfg.server.h2c;
    let tls = cfg.server.tls.clone();
    let prometheus_listen = cfg.telemetry.prometheus.as_ref().map(|p| p.listen.clone());
    let admin_cfg = cfg.telemetry.admin.clone();
    let graceful = cfg.server.graceful.clone();
    let concurrency = cfg.origin.concurrency.clone();
    let priorities = cfg.origin.priorities.clone();
    let purge_token = cfg.cache.purge.token.clone();
    let max_attempts = proxy_max_attempts(&cfg.origin.retry);

    // Span export, if it is configured. Built before the server so a bad
    // endpoint is a startup failure rather than a background service that
    // logs once and never works — the same reason every other unusable
    // setting in this project is refused at boot.
    let tracing_cfg = cfg.telemetry.tracing.clone();
    let deployment_id = cfg.deployment.id.clone();
    let spans = match tracing_cfg.otlp.as_ref() {
        Some(otlp) => {
            let mut resource = vec![
                (
                    "service.name".to_string(),
                    tracing_cfg
                        .service_name
                        .clone()
                        .unwrap_or_else(|| "tanod".to_string()),
                ),
                (
                    "service.version".to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                ),
            ];
            if let Some(id) = &deployment_id {
                resource.push(("deployment.id".to_string(), id.clone()));
            }
            match tanod::telemetry::otlp::build(otlp, resource) {
                Ok(pair) => Some(pair),
                Err(error) => {
                    eprintln!("error: telemetry.tracing.otlp: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    // Metric push, if configured. Same reasoning as spans: a bad endpoint or
    // a host with no CA bundle fails here, not a minute into serving.
    let metric_exporter = match cfg.telemetry.metrics.otlp.as_ref() {
        Some(otlp) => {
            let mut resource = vec![
                (
                    "service.name".to_string(),
                    tracing_cfg
                        .service_name
                        .clone()
                        .unwrap_or_else(|| "tanod".to_string()),
                ),
                (
                    "service.version".to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                ),
                (
                    "service.instance.id".to_string(),
                    tanod::telemetry::otlp_metrics::instance_id(),
                ),
            ];
            if let Some(id) = &deployment_id {
                resource.push(("deployment.id".to_string(), id.clone()));
            }
            match tanod::telemetry::otlp_metrics::build(otlp, resource) {
                Ok(exporter) => Some(exporter),
                Err(error) => {
                    eprintln!("error: telemetry.metrics.otlp: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    let policy = match PolicySnapshot::build(cfg, 1) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let policy = Arc::new(arc_swap::ArcSwap::from(policy));

    let admission = Arc::new(AdmissionController::new(
        concurrency.max,
        concurrency.queue.max,
        concurrency.queue.timeout.as_duration(),
        &priorities,
    ));
    // Create every configured route limiter now rather than on the route's
    // first request. Reload already does this, and without it the admin
    // status document reports no route limits at all until traffic arrives —
    // so the first thing an operator checks after a deploy says the policy
    // they just shipped is not there.
    {
        let snapshot = policy.load();
        let route_limits: Vec<(String, usize, usize, std::time::Duration)> = snapshot
            .config
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
        admission.apply_limits(
            concurrency.max,
            concurrency.queue.max,
            concurrency.queue.timeout.as_duration(),
            &priorities,
            &route_limits,
        );
    }
    // Create every tier series before traffic arrives. The request path only
    // updates the tier it actually changes, avoiding nine registry lookups per
    // request while keeping an idle tier visible as zero.
    for tier in admission.tier_limiters() {
        tanod::telemetry::metrics::LIMIT
            .with_label_values(&[tier.name()])
            .set(i64::try_from(tier.limit()).unwrap_or(i64::MAX));
        tanod::telemetry::metrics::QUEUE_DEPTH
            .with_label_values(&[tier.name()])
            .set(0);
        tanod::telemetry::metrics::IN_FLIGHT
            .with_label_values(&[tier.name()])
            .set(0);
    }

    tanod::telemetry::metrics::CONFIG_GENERATION.set(1);
    tanod::telemetry::metrics::CONFIG_FINGERPRINT
        .set(i64::try_from(policy.load().fingerprint).unwrap_or(i64::MAX));
    tanod::telemetry::metrics::set_build_info(policy.load().config.deployment.id.as_deref());
    // The ceilings the occupancy gauges are measured against. Published from
    // config rather than left for a dashboard to hardcode, so an alert cannot
    // go stale the first time somebody edits the budget.
    {
        let snapshot = policy.load();
        tanod::telemetry::metrics::CACHE_MAX_BYTES
            .set(i64::try_from(snapshot.config.cache.max_memory.get()).unwrap_or(i64::MAX));
        tanod::telemetry::metrics::SPOOL_MAX_BYTES
            .set(i64::try_from(snapshot.config.spool.max_memory.get()).unwrap_or(i64::MAX));
    }

    // Pingora's own server configuration, built from ours rather than from a
    // second file. The three fields that matter are the ones the zero-downtime
    // upgrade runs on: both processes have to agree on `upgrade_sock`, the pid
    // file is what every `kill -QUIT` in the documentation reads, and the two
    // grace periods bound how long the *old* process may keep serving after it
    // has handed its listeners over.
    let mut pingora_conf = pingora_core::server::configuration::ServerConf {
        pid_file: graceful.pid_file.clone(),
        upgrade_sock: graceful.upgrade_socket.clone(),
        daemon: flags.daemon,
        max_retries: max_attempts,
        // Teardown only. Pingora cancels every task still running when this
        // starts and then sleeps it out regardless, so it is kept short; the
        // time requests get to finish is the grace period below.
        graceful_shutdown_timeout_seconds: Some(1),
        ..Default::default()
    };
    // Tanod's signal watcher spends the load-balancer drain window before it
    // returns SIGTERM to Pingora, so new connections have stopped arriving by
    // then. Pingora's grace period is what follows: listeners are closed, and
    // requests already in flight keep running until it ends. It used to be 0,
    // with server.graceful.shutdown_timeout spent on the teardown instead —
    // which cancelled every in-flight request the moment the drain ended and
    // then waited the full timeout with nothing left to finish.
    pingora_conf.grace_period_seconds =
        Some(pingora_seconds(graceful.shutdown_timeout.as_duration()));
    // Pingora's socket handover is Linux-only: on every other platform
    // `get_fds_from` logs "Upgrade is not currently supported" and returns
    // `ECONNREFUSED`, which reads exactly like "no old process is listening"
    // and sends an operator looking for a problem that is not there. Refuse
    // up front and name the real reason, and point at the drain-based restart
    // that does work everywhere.
    // A supervised origin belongs to exactly one Tanod process: --upgrade would
    // start a second origin on the same port while the first still runs, and
    // --daemon forks away from the child it just started.
    if origin_command.is_some() && (flags.upgrade || flags.daemon) {
        eprintln!(
            "error: --upgrade and --daemon cannot be used with origin.command. Restart instead: \
             SIGTERM drains Tanod, then stops the origin."
        );
        return ExitCode::FAILURE;
    }
    if flags.upgrade && !cfg!(target_os = "linux") {
        eprintln!(
            "error: --upgrade is not supported on this platform. Pingora can only pass \n\
             listening sockets between processes on Linux.\n\n\
             Use the drain-based restart instead: SIGUSR1 to this process, wait for your \n\
             load balancer to withdraw it (telemetry.admin /health/ready answers 503 \n\
             immediately), then SIGTERM and start the new process.\n\
             See docs/OPERATIONS.md."
        );
        return ExitCode::FAILURE;
    }

    let opt = pingora_core::server::configuration::Opt {
        upgrade: flags.upgrade,
        daemon: flags.daemon,
        test: flags.test,
        nocapture: false,
        conf: None,
    };
    let mut server = pingora_core::server::Server::new_with_opt_and_conf(opt, pingora_conf);
    // Under `--upgrade` this is where the listening sockets are taken over
    // from the running process, and under `--test` it exits zero once it has
    // proved the process can start.
    server.bootstrap();

    // One pool, shared by the proxy and the health checker, so a probe result
    // is visible to routing immediately.
    let snapshot = policy.load();
    let upstreams = match UpstreamPool::new(
        &snapshot.config.origin.upstreams,
        snapshot.config.origin.load_balancing,
        &snapshot.config.origin.breaker,
    ) {
        Ok(pool) => Arc::new(pool),
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let health_cfg = snapshot.config.health.clone();
    drop(snapshot);

    // Without an active checker, configured backends are assumed available.
    // With one, they remain unknown/unhealthy until a probe actually passes,
    // which makes `require_healthy_upstream` truthful during startup.
    if health_cfg.is_none() {
        upstreams.assume_healthy();
    }
    for backend in upstreams.backends() {
        tanod::telemetry::metrics::UPSTREAM_HEALTHY
            .with_label_values(&[&backend.address])
            .set(i64::from(upstreams.is_healthy(backend.id)));
        tanod::telemetry::metrics::UPSTREAM_EJECTED
            .with_label_values(&[&backend.address])
            .set(0);
        tanod::telemetry::metrics::UPSTREAM_IN_FLIGHT
            .with_label_values(&[&backend.address])
            .set(0);
        tanod::telemetry::metrics::UPSTREAM_LATENCY_EWMA
            .with_label_values(&[&backend.address])
            .set(0);
    }

    // Drain state is shared: the admin endpoints read it, the background
    // watcher sets it for SIGUSR1, and the server signal watcher sets it before
    // allowing SIGTERM to reach Pingora.
    let drain = Arc::new(DrainState::new());
    server.add_service(pingora_core::services::background::background_service(
        "drain",
        DrainWatcher::new(drain.clone()),
    ));
    eprintln!(
        "  drain without exiting with: kill -USR1 <pid>  (drain window {:?})",
        graceful.drain_period.as_duration()
    );

    let span_sink = match spans {
        Some((sink, exporter)) => {
            server.add_service(pingora_core::services::background::background_service(
                "otlp", exporter,
            ));
            Some(sink)
        }
        None => None,
    };
    if let Some(exporter) = metric_exporter {
        eprintln!("  exporting metrics to {}", exporter.endpoint().url());
        server.add_service(pingora_core::services::background::background_service(
            "otlp-metrics",
            exporter,
        ));
    }

    let resolve_interval = policy.load().config.origin.resolve_interval.as_duration();
    for backend in upstreams.backends() {
        tanod::telemetry::metrics::UPSTREAM_ADDRESSES
            .with_label_values(&[&backend.address])
            .set(i64::try_from(backend.sockets().len()).unwrap_or(i64::MAX));
    }
    if !resolve_interval.is_zero() {
        eprintln!("  upstream re-resolution: every {resolve_interval:?}");
        server.add_service(pingora_core::services::background::background_service(
            "resolver",
            tanod::upstream::resolver::Resolver::new(upstreams.clone(), resolve_interval),
        ));
    }

    if let Some(health) = health_cfg {
        eprintln!(
            "  health checks: {} every {:?}",
            health.path,
            health.interval.as_duration()
        );
        server.add_service(pingora_core::services::background::background_service(
            "health",
            HealthChecker::new(upstreams.clone(), &health),
        ));
    }

    let tanod = match Tanod::new(
        policy.clone(),
        admission.clone(),
        upstreams.clone(),
        span_sink,
    ) {
        Ok(tanod) => tanod,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Registered here rather than earlier because the reloader needs the
    // response cache, which `Tanod::new` creates: a `deployment.id` change
    // has to reclaim the previous build's entries, and a reloader with no
    // store would silently skip that.
    server.add_service(pingora_core::services::background::background_service(
        "reload",
        Reloader::new(path.to_string(), policy.clone(), admission.clone())
            .with_store(tanod.store()),
    ));
    eprintln!("  reload config with: kill -HUP <pid>");

    // Taken before the proxy is moved into its service. These are handles to
    // the same live state the request path uses, not copies: a status document
    // assembled from a startup snapshot would report zeros forever.
    let admin_listen = admin_cfg.as_ref().map(|a| a.listen.clone());
    let admin = admin_cfg.map(|admin_cfg| Admin {
        started: std::time::Instant::now(),
        config_path: path.to_string(),
        policy: policy.clone(),
        admission: admission.clone(),
        upstreams: upstreams.clone(),
        store: tanod.store(),
        spool: tanod.spool_budget(),
        upgrades: tanod.upgrade_limiter(),
        retry: tanod.retry_budget(),
        purge_token: purge_token.clone(),
        drain: drain.clone(),
        require_healthy_upstream: admin_cfg.require_healthy_upstream,
    });
    let mut service = pingora_proxy::http_proxy_service(&server.configuration, tanod);

    // HTTP/2 over cleartext. Pingora peeks for the connection preface, so this
    // listener still serves HTTP/1.1 clients; it only decides whether an h2
    // preface is honoured or answered as a malformed HTTP/1.1 request.
    if h2c {
        // Refusing to start beats starting without the protocol that was
        // asked for: a listener that silently speaks only HTTP/1.1 is a
        // misconfiguration nobody notices until a client does.
        let Some(logic) = service.app_logic_mut() else {
            eprintln!("error: could not enable h2c: proxy service has no app logic");
            return ExitCode::FAILURE;
        };
        logic
            .server_options
            .get_or_insert_with(Default::default)
            .h2c = true;
        eprintln!("  h2c enabled on {listen}");
    }

    service.add_tcp(&listen);

    // Validation has already refused a `server.tls` block in a build without
    // the feature, so the `not(tls)` arm is unreachable in practice. It is
    // written out anyway: an unreachable branch that fails loudly costs
    // nothing, and the alternative — a `cfg` that silently drops the listener
    // — is the failure this whole arrangement exists to avoid.
    if let Some(tls) = &tls {
        #[cfg(feature = "tls")]
        match tls_settings(tls) {
            Ok(settings) => {
                service.add_tls_with_settings(&tls.listen, None, settings);
                eprintln!("tanod listening on {} (TLS)", tls.listen);
            }
            Err(error) => {
                eprintln!("error: server.tls: {error}");
                return ExitCode::FAILURE;
            }
        }
        #[cfg(not(feature = "tls"))]
        {
            eprintln!("error: server.tls: {}", tls_settings(tls).unwrap_err());
            return ExitCode::FAILURE;
        }
    }

    server.add_service(service);

    if let Some(addr) = &prometheus_listen {
        let mut metrics = pingora_prometheus::prometheus_http_service();
        metrics.add_tcp(addr);
        server.add_service(metrics);
        eprintln!("tanod metrics on {addr}/metrics");
    }

    if let (Some(admin), Some(addr)) = (admin, admin_listen) {
        let mut service = pingora_core::services::listening::Service::new(
            "admin".to_string(),
            pingora_core::apps::http_app::HttpServer::new_app(admin),
        );
        service.add_tcp(&addr);
        server.add_service(service);
        eprintln!("tanod admin on {addr}  (/health/live, /health/ready, /status)");
    }

    eprintln!("tanod listening on {listen}");
    eprintln!("  origin concurrency ceiling: {}", concurrency.max);
    let mut shutdown_signal =
        DrainShutdownSignalWatch::new(drain, graceful.drain_period.as_duration());

    // The origin, if Tanod supervises it: started last, once everything above
    // has been built, so a bad config never leaves an orphaned server behind;
    // and only served once it accepts connections.
    let supervisor = match &origin_command {
        Some(command) => {
            shutdown_signal = shutdown_signal.with_supervised_origin();
            eprintln!(
                "  starting origin: {} (waiting up to {:?} for {})",
                command.args.join(" "),
                command.ready_timeout.as_duration(),
                origin_upstream
            );
            match tanod::supervise::start(command, &origin_upstream, |status| {
                // Nothing behind Tanod any more: exit so the platform restarts
                // the pair, with the origin's code (never 0, since it should
                // not have stopped).
                log::error!("the origin exited ({status}); stopping tanod");
                let code = tanod::supervise::exit_code(status);
                std::process::exit(if code == 0 { 1 } else { code });
            }) {
                Ok(supervisor) => {
                    eprintln!("  origin is up (pid {})", supervisor.pid());
                    Some(supervisor)
                }
                Err(error) => {
                    eprintln!("error: origin.command: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

    let run_args = pingora_core::server::RunArgs {
        shutdown_signal: Box::new(shutdown_signal),
    };
    server.run(run_args);

    // Pingora has drained and finished every in-flight request; only now is
    // it safe to stop the origin those requests were going to.
    if let Some(supervisor) = supervisor {
        log::info!("stopping the origin (pid {})", supervisor.pid());
        match supervisor.stop() {
            Some(status) => log::info!("the origin exited ({status})"),
            None => log::warn!("the origin did not report an exit status"),
        }
    }
    ExitCode::SUCCESS
}

/// Pingora's lifecycle configuration is expressed in whole seconds. Round up
/// rather than truncate: `500ms` must never become an immediate shutdown, and
/// a configured safety window should be a floor rather than an accidental
/// shorter value.
fn pingora_seconds(duration: std::time::Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() != 0))
}

/// Build the listener's TLS settings.
///
/// Two builds exist. With the `tls` feature this compiles against rustls and
/// terminates TLS in process; without it the function refuses, and validation
/// refuses earlier still. Neither build silently ignores a `server.tls` block:
/// a config that asks for TLS and gets cleartext is the worst possible outcome,
/// and the same reasoning is why every unimplemented key in this project is
/// rejected rather than skipped.
#[cfg(feature = "tls")]
fn tls_settings(
    tls: &tanod::config::schema::ServerTls,
) -> std::result::Result<pingora_core::listeners::tls::TlsSettings, String> {
    let mut settings = pingora_core::listeners::tls::TlsSettings::intermediate(&tls.cert, &tls.key)
        .map_err(|error| {
            format!(
                "could not load cert `{}`/key `{}`: {error}",
                tls.cert, tls.key
            )
        })?;
    if tls.h2 {
        // Offers `h2` alongside `http/1.1`, so an HTTP/1.1-only client is
        // never locked out of the TLS listener.
        settings.enable_h2();
    }
    Ok(settings)
}

#[cfg(not(feature = "tls"))]
fn tls_settings(
    _tls: &tanod::config::schema::ServerTls,
) -> std::result::Result<std::convert::Infallible, String> {
    Err(
        "this binary was built without the `tls` feature; rebuild with \
         `cargo build --features tls` or terminate TLS in front of Tanod"
            .to_string(),
    )
}

fn check(path: &str) -> ExitCode {
    match tanod::config::load(path) {
        Ok(cfg) => {
            let policy = match tanod::policy::PolicySnapshot::build(cfg, 1) {
                Ok(policy) => policy,
                Err(e) => {
                    eprintln!("error: invalid configuration in {path}");
                    eprintln!("  caused by: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let cfg = &policy.config;
            let routes = cfg.routes.len();
            let upstreams = cfg.origin.upstreams.len();
            println!("ok: {path}");
            println!(
                "  config schema v{} (tanod {})",
                cfg.version,
                env!("CARGO_PKG_VERSION")
            );
            println!("  {upstreams} upstream(s), {routes} route(s)");
            if let Some(command) = &cfg.origin.command {
                println!(
                    "  supervising origin: {} (ready within {:?}, stopped {:?} after SIGTERM)",
                    command.args.join(" "),
                    command.ready_timeout.as_duration(),
                    command.stop_timeout.as_duration()
                );
            }
            match cfg.mode {
                tanod::config::schema::Mode::Observe => println!(
                    "  mode: observe — classification and telemetry only; admission, cache, coalescing, and spooling are disabled"
                ),
                tanod::config::schema::Mode::Protect => println!("  mode: protect"),
            }
            println!(
                "  global origin concurrency: {}",
                cfg.origin.concurrency.max
            );
            if let Some(group) = &cfg.capacity {
                let allocated = cfg.origin.concurrency.max * group.replicas;
                println!(
                    "  replica-group capacity: {allocated}/{} allocated across at most {} replica(s)",
                    group.global_max, group.replicas
                );
            }
            for r in &cfg.routes {
                let overrides = r.cache.as_ref().is_some_and(|c| c.override_origin);
                if overrides {
                    println!("  route `{}` overrides origin cache directives", r.id);
                }
            }
            // Origin resilience. Each of these changes where requests go or
            // how many of them the origin sees, so `check` says what is on
            // and what the consequence is rather than leaving it in the file.
            if !cfg.origin.priorities.is_uniform() {
                let p = &cfg.origin.priorities;
                let max = cfg.origin.concurrency.max;
                // Saturating: `concurrency.max` is bounded only by tokio's
                // semaphore maximum, which is large enough that `max * 100`
                // is not obviously in range.
                let share = |percent: u32| max.saturating_mul(percent as usize) / 100;
                println!(
                    "  priority ceilings: high {} of {max}, normal {}, low {} — the rest of the \
                     ceiling is reserved for higher tiers",
                    share(p.high),
                    share(p.normal),
                    share(p.low),
                );
            }
            if cfg.origin.breaker.enabled {
                println!(
                    "  circuit breakers on: a backend failing {}% of at least {} requests in {:?} \
                     is ejected for {:?}, up to {}% of the pool at once",
                    cfg.origin.breaker.failure_percent,
                    cfg.origin.breaker.min_requests,
                    cfg.origin.breaker.window.as_duration(),
                    cfg.origin.breaker.open_for.as_duration(),
                    cfg.origin.breaker.max_ejected_percent,
                );
            }
            if cfg.origin.retry.enabled {
                println!(
                    "  retries on: safe methods only, up to {} attempts, capped at {}% of origin \
                     requests in {:?} (never fewer than {})",
                    cfg.origin.retry.max_attempts,
                    cfg.origin.retry.budget_percent,
                    cfg.origin.retry.window.as_duration(),
                    cfg.origin.retry.budget_min,
                );
            }
            if cfg.cache.purge.token.is_some() {
                println!(
                    "  purge endpoint ENABLED: POST /purge on the admin listener invalidates \
                     cache entries by tag. Anyone holding cache.purge.token can make this \
                     origin re-render on demand"
                );
            }
            if cfg
                .cache
                .tag_header
                .eq_ignore_ascii_case("x-next-cache-tags")
            {
                println!(
                    "  reading invalidation tags from Next.js's own x-next-cache-tags; that \
                     header is only emitted with NEXT_PRIVATE_MINIMAL_MODE=1 and only for \
                     statically generated routes. See docs/OPERATIONS.md"
                );
            }
            // Everything below trades a safety property for convenience.
            // `check` is the last place someone reads before deploying, so it
            // says so out loud rather than leaving it in the file.
            if cfg.server.trusted_proxies.from.is_empty() {
                println!(
                    "  no server.trusted_proxies: forwarded client addresses and schemes \
                     are ignored, and the connection peer is treated as the client"
                );
            } else {
                println!(
                    "  trusting forwarded headers from {} block(s)",
                    cfg.server.trusted_proxies.from.len()
                );
            }
            if let Some(tls) = &cfg.origin.tls
                && (!tls.verify_cert || !tls.verify_hostname)
            {
                println!(
                    "  WARNING: origin.tls does not verify the origin's certificate; \
                     the connection is encrypted but not authenticated"
                );
            }
            // Operability surface, stated out loud for the same reason the
            // trust settings are: `check` is the last thing anyone reads
            // before deploying.
            match &cfg.telemetry.admin {
                Some(admin) => {
                    println!(
                        "  admin endpoints on {} (/health/live, /health/ready, /status)",
                        admin.listen
                    );
                    if admin.require_healthy_upstream {
                        println!(
                            "    readiness FAILS when no upstream is healthy; make sure something \
                             upstream of Tanod can route around this instance, or a degraded \
                             origin takes every replica out of rotation at once"
                        );
                    }
                    if admin
                        .listen
                        .parse::<std::net::SocketAddr>()
                        .is_ok_and(|a| a.ip().is_unspecified())
                    {
                        println!(
                            "    WARNING: bound to an unspecified address, so it is reachable on \
                             every interface. It publishes backend health, cache occupancy and \
                             the config generation; bind it to loopback or a private address"
                        );
                    }
                }
                None => println!(
                    "  no telemetry.admin: there is no readiness endpoint, so a load balancer \
                     cannot tell when this instance is draining"
                ),
            }
            match &cfg.telemetry.tracing.otlp {
                Some(otlp) => println!(
                    "  exporting spans to {} ({:?} batches of up to {})",
                    otlp.endpoint,
                    otlp.interval.as_duration(),
                    otlp.max_batch
                ),
                None => println!(
                    "  no telemetry.tracing.otlp: trace ids are still generated, logged and \
                     forwarded to the origin, but no spans leave this process"
                ),
            }
            if let Some(otlp) = &cfg.telemetry.metrics.otlp {
                println!(
                    "  pushing metrics to {} every {:?}",
                    otlp.endpoint,
                    otlp.interval.as_duration()
                );
            }
            if cfg.telemetry.tracing.trust_incoming
                == tanod::config::schema::TrustIncoming::FromTrustedProxies
            {
                println!(
                    "  WARNING: traceparent is trusted from server.trusted_proxies; those proxies \
                     must strip or replace client-supplied traceparent/tracestate headers, or an \
                     internet client can choose trace ids and force parent-based sampling"
                );
            }
            let drain_period = cfg.server.graceful.drain_period.as_duration();
            let shutdown_timeout = std::time::Duration::from_secs(pingora_seconds(
                cfg.server.graceful.shutdown_timeout.as_duration(),
            ));
            // Plus the one-second runtime teardown that follows the window in
            // which in-flight requests finish.
            let stop_budget = drain_period
                .saturating_add(shutdown_timeout)
                .saturating_add(std::time::Duration::from_secs(1));
            println!(
                "  graceful restart: pid {}, socket {}",
                cfg.server.graceful.pid_file, cfg.server.graceful.upgrade_socket,
            );
            // Stated as one number because that is the number a supervisor is
            // configured with, and because `shutdown_timeout` is a floor
            // rather than a ceiling — a SIGTERM costs the full sum even on an
            // idle process. See the note on `Graceful` in the schema.
            println!(
                "    a SIGTERM takes about {stop_budget:?} ({drain_period:?} drain + \
                 {shutdown_timeout:?} for in-flight requests + 1s teardown), on an idle \
                 process too",
            );
            if stop_budget > std::time::Duration::from_secs(30) {
                println!(
                    "    WARNING: that exceeds Kubernetes' default \
                     terminationGracePeriodSeconds of 30, so the pod will be SIGKILLed \
                     part-way through the drain. Raise the supervisor's timeout to at \
                     least {}s or lower these two",
                    stop_budget.as_secs().saturating_add(5)
                );
            }
            if cfg.upgrade.enabled {
                println!(
                    "  Upgrade/WebSocket proxying enabled, up to {} concurrent connections",
                    cfg.upgrade.max_concurrent
                );
            }
            if cfg.spool.enabled
                || cfg
                    .routes
                    .iter()
                    .any(|r| r.spool.as_ref().and_then(|spool| spool.enabled) == Some(true))
            {
                println!(
                    "  response spooling enabled: up to {} bytes per response, {} bytes overall; \
                     spooled routes lose progressive rendering",
                    cfg.spool.max_body.get(),
                    cfg.spool.max_memory.get()
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                eprintln!("  caused by: {s}");
                src = s.source();
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_defaults_make_a_safe_valid_config() {
        let options = InitOptions::default();
        let text = render_initial_config(&options);
        let config: tanod::config::schema::Config = serde_saphyr::from_str(&text).unwrap();

        tanod::config::validation::validate(&config).unwrap();
        tanod::policy::PolicySnapshot::build(config, 1).unwrap();
        assert!(text.contains("class: private_dynamic"));
        assert!(text.contains("enabled: false"));
    }

    #[test]
    fn init_options_override_the_single_server_defaults() {
        let args = vec![
            "init".to_string(),
            "--config".to_string(),
            "/tmp/custom.yml".to_string(),
            "--upstream".to_string(),
            "app.internal:4000".to_string(),
            "--listen".to_string(),
            "0.0.0.0:8088".to_string(),
            "--concurrency".to_string(),
            "24".to_string(),
            "--force".to_string(),
        ];

        let options = parse_init_options(&args).unwrap();
        assert_eq!(options.config, PathBuf::from("/tmp/custom.yml"));
        assert_eq!(options.upstream, "app.internal:4000");
        assert_eq!(options.listen, "0.0.0.0:8088");
        assert_eq!(options.concurrency, 24);
        assert!(options.force);
    }

    #[test]
    fn init_rejects_zero_concurrency_and_missing_values() {
        let zero = vec![
            "init".to_string(),
            "--concurrency".to_string(),
            "0".to_string(),
        ];
        assert_eq!(
            parse_init_options(&zero).unwrap_err(),
            "--concurrency must be greater than zero"
        );

        let missing = vec!["init".to_string(), "--upstream".to_string()];
        assert_eq!(
            parse_init_options(&missing).unwrap_err(),
            "--upstream requires a value"
        );
    }

    #[test]
    fn pingora_timeouts_round_fractional_seconds_up() {
        assert_eq!(pingora_seconds(std::time::Duration::ZERO), 0);
        assert_eq!(pingora_seconds(std::time::Duration::from_millis(500)), 1);
        assert_eq!(pingora_seconds(std::time::Duration::from_secs(1)), 1);
        assert_eq!(pingora_seconds(std::time::Duration::from_millis(1500)), 2);
    }

    #[test]
    fn pingora_attempt_ceiling_tracks_the_retry_policy_instead_of_its_default() {
        let mut retry = tanod::config::schema::Retry::default();
        assert_eq!(proxy_max_attempts(&retry), 1);
        retry.enabled = true;
        retry.max_attempts = 23;
        assert_eq!(proxy_max_attempts(&retry), 23);
    }

    #[test]
    fn check_rejects_a_matcher_that_cannot_be_compiled() {
        let path = std::env::temp_dir().join(format!(
            "tanod-check-{}-{}.yaml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(
            &path,
            "version: 1\norigin:\n  upstreams: [\"127.0.0.1:3000\"]\nroutes:\n  - id: bad\n    match: \"[\"\n",
        )
        .unwrap();
        let result = check(path.to_str().unwrap());
        let _ = std::fs::remove_file(path);
        assert_eq!(result, ExitCode::FAILURE);
    }
}
