//! Startup refuses to run an unsafe configuration.
//!
//! Every check here corresponds to a way the config can be *syntactically*
//! valid while describing something that leaks data or defeats the point of
//! running Tanod at all. Failing at boot is the cheapest place to catch them.

use super::schema::*;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

pub type Result<T> = std::result::Result<T, ValidationError>;

fn err(msg: impl Into<String>) -> ValidationError {
    ValidationError(msg.into())
}

pub fn validate(cfg: &Config) -> Result<()> {
    if cfg.version != super::SCHEMA_VERSION {
        // Refused, never coerced. The format is pre-1.0 and a future version
        // may reinterpret a key that exists today, so a binary that guessed
        // would be applying a policy nobody wrote. Both numbers are in the
        // message because the useful next question is "which binary is this".
        return Err(err(format!(
            "config schema version {} is not supported; this build of tanod understands \
             version {}. See docs/CONFIG-SCHEMA.md for the compatibility rules and the \
             migration notes",
            cfg.version,
            super::SCHEMA_VERSION
        )));
    }
    if cfg.origin.upstreams.is_empty() {
        return Err(err(
            "origin.upstreams is empty; there is nothing to proxy to",
        ));
    }
    if cfg.origin.concurrency.max == 0 {
        return Err(err(
            "origin.concurrency.max is 0, which admits nothing; omit the key to take the default",
        ));
    }
    check_concurrency_max(cfg.origin.concurrency.max, "origin.concurrency.max")?;
    validate_capacity_group(cfg)?;
    if cfg.cache.max_memory.get() == 0 {
        return Err(err("cache.max_memory must be greater than zero"));
    }
    if cfg.cache.max_body_size.get() == 0 {
        return Err(err("cache.max_body_size must be greater than zero"));
    }
    if cfg.cache.max_body_size > cfg.cache.max_memory {
        return Err(err(format!(
            "cache.max_body_size ({} bytes) exceeds cache.max_memory ({} bytes)",
            cfg.cache.max_body_size.get(),
            cfg.cache.max_memory.get()
        )));
    }
    if !(400..=599).contains(&cfg.overload.status) {
        return Err(err(format!(
            "overload.status {} is not a 4xx or 5xx; an overload response must be an error",
            cfg.overload.status
        )));
    }
    validate_overload_page(&cfg.overload.page)?;
    validate_origin_command(&cfg.origin)?;
    if cfg.deployment.id.is_some() && cfg.deployment.id_header.is_some() {
        return Err(err(
            "deployment.id and deployment.id_header are both set; pick one source of truth",
        ));
    }
    if cfg.deployment.id.as_ref().is_some_and(|id| {
        id.chars()
            .any(|c| matches!(c, '\u{1d}' | '\u{1e}' | '\u{1f}'))
    }) {
        return Err(err("deployment.id contains a reserved cache-key separator"));
    }

    validate_listen(&cfg.server.listen, "server.listen")?;
    validate_server_tls(cfg)?;
    validate_trusted_proxies(cfg)?;
    validate_origin_tls(cfg)?;
    validate_spool(cfg)?;
    validate_upgrade(cfg)?;
    if let Some(prometheus) = &cfg.telemetry.prometheus {
        validate_listen(&prometheus.listen, "telemetry.prometheus.listen")?;
    }
    validate_graceful(cfg)?;
    validate_admin(cfg)?;
    validate_tracing(cfg)?;
    validate_metrics_export(cfg)?;
    for upstream in &cfg.origin.upstreams {
        validate_upstream(upstream)?;
    }
    if let Some(health) = &cfg.health {
        if !health.path.starts_with('/') {
            return Err(err("health.path must start with `/`"));
        }
        if health.interval == super::units::Dur::ZERO || health.timeout == super::units::Dur::ZERO {
            return Err(err(
                "health.interval and health.timeout must be greater than zero",
            ));
        }
        if health.healthy_after == 0 || health.unhealthy_after == 0 {
            return Err(err(
                "health.healthy_after and health.unhealthy_after must be greater than zero",
            ));
        }
    }
    if cfg
        .telemetry
        .admin
        .as_ref()
        .is_some_and(|admin| admin.require_healthy_upstream)
        && cfg.health.is_none()
    {
        return Err(err(
            "telemetry.admin.require_healthy_upstream is true but no health check is configured; \
             readiness could never establish that an upstream passed",
        ));
    }

    check_unimplemented(cfg)?;
    check_coalesce_wait(cfg)?;
    check_queue(&cfg.origin.concurrency, "origin.concurrency")?;
    validate_purge(cfg)?;
    validate_tag_header(cfg)?;
    validate_breaker(cfg)?;
    validate_retry(cfg)?;
    validate_priorities(cfg)?;

    let mut seen: HashSet<&str> = HashSet::new();
    for route in &cfg.routes {
        if !seen.insert(route.id.as_str()) {
            return Err(err(format!("duplicate route id `{}`", route.id)));
        }
        check_route(route, cfg)?;
    }
    Ok(())
}

fn validate_origin_command(origin: &Origin) -> Result<()> {
    let Some(command) = &origin.command else {
        return Ok(());
    };
    if command.args.is_empty() || command.args[0].trim().is_empty() {
        return Err(err("origin.command.args is empty; give the program to run"));
    }
    // A supervised origin is the process beside Tanod. Several upstreams, or
    // a remote one, would mean supervising one thing and proxying to another.
    let [upstream] = origin.upstreams.as_slice() else {
        return Err(err(
            "origin.command needs exactly one upstream: the address the command listens on",
        ));
    };
    let Some((host, port)) = crate::supervise::split_upstream(upstream) else {
        return Err(err(format!(
            "origin.command: upstream {upstream} is not host:port"
        )));
    };
    if !crate::supervise::is_loopback(&host) {
        return Err(err(format!(
            "origin.command: upstream {upstream} is not a loopback address; a supervised \
             origin runs beside Tanod, so use 127.0.0.1:{port}"
        )));
    }
    for name in command.env.keys() {
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(err(format!(
                "origin.command.env: {name:?} is not a valid environment variable name"
            )));
        }
    }
    let ready = command.ready_timeout.as_duration();
    if ready < Duration::from_secs(1) || ready > Duration::from_secs(600) {
        return Err(err(
            "origin.command.ready_timeout must be between 1s and 10m",
        ));
    }
    let stop = command.stop_timeout.as_duration();
    if stop < Duration::from_secs(1) || stop > Duration::from_secs(300) {
        return Err(err("origin.command.stop_timeout must be between 1s and 5m"));
    }
    Ok(())
}

/// Ceiling on a custom overload page. It is held in memory and written on
/// every shed, which happens precisely when the proxy is busiest.
pub const MAX_OVERLOAD_PAGE_BYTES: usize = 64 * 1024;

fn validate_overload_page(page: &OverloadPage) -> Result<()> {
    let refresh = page.refresh.as_duration();
    if refresh < Duration::from_secs(1) || refresh > Duration::from_secs(600) {
        return Err(err(format!(
            "overload.page.refresh is {refresh:?}; it must be between 1s and 10m, because a \
             page that reloads faster than once a second only adds load"
        )));
    }
    if page.lang.is_empty()
        || page.lang.len() > 35
        || !page
            .lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(err(format!(
            "overload.page.lang {:?} is not a language tag such as \"en\" or \"fil-PH\"",
            page.lang
        )));
    }
    if page.title.trim().is_empty() || page.title.chars().count() > 200 {
        return Err(err("overload.page.title must be 1 to 200 characters"));
    }
    if page.message.chars().count() > 2000 {
        return Err(err("overload.page.message must be at most 2000 characters"));
    }
    if let (Some(file), Some(template)) = (&page.file, &page.template)
        && template.len() > MAX_OVERLOAD_PAGE_BYTES
    {
        return Err(err(format!(
            "overload.page.file {file} is larger than {} KiB",
            MAX_OVERLOAD_PAGE_BYTES / 1024
        )));
    }
    Ok(())
}

fn validate_capacity_group(cfg: &Config) -> Result<()> {
    let Some(group) = &cfg.capacity else {
        return Ok(());
    };
    if group.global_max == 0 {
        return Err(err("capacity.global_max must be greater than zero"));
    }
    if group.replicas == 0 {
        return Err(err("capacity.replicas must be greater than zero"));
    }
    let allocated = cfg
        .origin
        .concurrency
        .max
        .checked_mul(group.replicas)
        .ok_or_else(|| err("capacity allocation overflows this platform"))?;
    if allocated > group.global_max {
        return Err(err(format!(
            "origin.concurrency.max ({}) across capacity.replicas ({}) allocates {allocated}, \
             which exceeds capacity.global_max ({})",
            cfg.origin.concurrency.max, group.replicas, group.global_max
        )));
    }
    Ok(())
}

/// Reject options that parse but do nothing.
///
/// A config key that is accepted and then silently ignored is worse than one
/// that does not exist: it lets someone ship believing a protection is on. If
/// these get implemented, delete the corresponding check.
fn check_unimplemented(cfg: &Config) -> Result<()> {
    if !cfg.cache.respect_origin {
        return Err(err(
            "cache.respect_origin: false is not implemented; origin cache directives are \
             always honoured except where a route sets cache.override_origin",
        ));
    }
    if cfg.deployment.id_header.is_some() {
        return Err(err(
            "deployment.id_header is not implemented — a cache key is built before the \
             response exists, so the id cannot come from a response header; use \
             deployment.id instead",
        ));
    }
    Ok(())
}

/// TLS asked for and TLS delivered must be the same thing.
///
/// The failure this exists to prevent is a binary built without the `tls`
/// feature accepting a `server.tls` block and then serving nothing on that
/// address — an operator would see a valid config, a running process, and a
/// port that refuses connections, with no line anywhere saying why.
fn validate_server_tls(cfg: &Config) -> Result<()> {
    let Some(tls) = &cfg.server.tls else {
        return Ok(());
    };
    if !cfg!(feature = "tls") {
        return Err(err(
            "server.tls is set but this binary was built without the `tls` feature; rebuild with \
             `cargo build --features tls`, or remove server.tls and terminate TLS in front of \
             Tanod",
        ));
    }
    validate_listen(&tls.listen, "server.tls.listen")?;
    if tls.listen == cfg.server.listen {
        return Err(err(format!(
            "server.tls.listen and server.listen are both `{}`; one address cannot serve cleartext \
             and TLS at once",
            tls.listen
        )));
    }
    if tls.cert.is_empty() || tls.key.is_empty() {
        return Err(err("server.tls.cert and server.tls.key must both be set"));
    }
    // Checked here rather than at bind time so that `tanod check` catches a
    // missing certificate before a deploy rather than after one.
    for (path, label) in [(&tls.cert, "cert"), (&tls.key, "key")] {
        if !std::path::Path::new(path).exists() {
            return Err(err(format!("server.tls.{label} `{path}` does not exist")));
        }
    }
    #[cfg(feature = "tls")]
    validate_server_tls_material(&tls.cert, &tls.key)?;
    Ok(())
}

#[cfg(feature = "tls")]
fn validate_server_tls_material(cert_path: &str, key_path: &str) -> Result<()> {
    pingora_core::tls::install_default_crypto_provider();
    let material =
        pingora_core::tls::load_certs_and_key_files(cert_path, key_path).map_err(|error| {
            err(format!(
                "could not read server TLS certificate or key: {error}"
            ))
        })?;
    let Some((certs, key)) = material else {
        return Err(err(
            "server TLS files contain no usable certificate/private-key pair",
        ));
    };

    // Mirror Pingora's rustls listener construction here. Pingora 0.8 builds
    // this later through an infallible API that panics on malformed or
    // incompatible material; validation turns that startup panic into a
    // normal `tanod check` error.
    pingora_core::tls::ServerConfig::builder_with_protocol_versions(&[
        &pingora_core::tls::version::TLS12,
        &pingora_core::tls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map(|_| ())
    .map_err(|error| err(format!("server TLS certificate/key are invalid: {error}")))
}

/// A trust list that cannot be parsed is a trust list that does not protect
/// anything, and the symptom — forwarded headers silently ignored — looks
/// exactly like a working deployment until someone reads the logs.
fn validate_trusted_proxies(cfg: &Config) -> Result<()> {
    let proxies = &cfg.server.trusted_proxies;
    for block in &proxies.from {
        crate::net::forwarded::Cidr::parse(block)
            .map_err(|error| err(format!("server.trusted_proxies.from: {error}")))?;
    }
    // Naming a source without naming anyone to believe is a policy that reads
    // as "trust X-Forwarded-For" and behaves as "trust nothing".
    let reads_headers =
        proxies.client_ip != ForwardedSource::None || proxies.scheme != ForwardedSource::None;
    if proxies.from.is_empty()
        && reads_headers
        && cfg.server.trusted_proxies != TrustedProxies::default()
    {
        return Err(err(
            "server.trusted_proxies names a client_ip or scheme source but `from` is empty, so no \
             peer is ever trusted and neither is read; list the CIDR blocks your load balancer \
             connects from",
        ));
    }
    Ok(())
}

fn validate_origin_tls(cfg: &Config) -> Result<()> {
    let Some(tls) = &cfg.origin.tls else {
        // ALPN only exists inside a TLS handshake. Asking to negotiate over
        // cleartext is a request that cannot be honoured, and the honest
        // failure is here rather than as a connection error per request.
        if cfg.origin.http_version == OriginHttpVersion::Auto {
            return Err(err(
                "origin.http_version: auto negotiates over ALPN, which requires origin.tls; over \
                 cleartext choose http1 or http2 explicitly",
            ));
        }
        return Ok(());
    };
    if !cfg!(feature = "tls") {
        return Err(err(
            "origin.tls is set but this binary was built without the `tls` feature; rebuild with \
             `cargo build --features tls`",
        ));
    }
    if tls.sni.trim().is_empty() {
        return Err(err(
            "origin.tls.sni is empty; a peer with no SNI cannot be hostname-verified",
        ));
    }
    // Verifying the name against a chain nobody checked verifies nothing.
    if tls.verify_hostname && !tls.verify_cert {
        return Err(err(
            "origin.tls sets verify_hostname without verify_cert; a hostname is only meaningful once \
             the chain that vouches for it has been checked",
        ));
    }
    if tls.ca.is_some() {
        return Err(err(
            "origin.tls.ca is not implemented: Pingora 0.8's rustls connector does not read the \
             per-peer CA store (its connect path carries an explicit TODO and never calls \
             peer.get_ca()), so naming a CA here would verify against the system roots anyway. Add \
             the CA to the platform trust store, or point SSL_CERT_FILE / SSL_CERT_DIR at it — both \
             are honoured — or set verify_cert: false if the origin is reachable only over a private \
             network",
        ));
    }
    Ok(())
}

fn validate_spool(cfg: &Config) -> Result<()> {
    if cfg.spool.max_body.get() == 0 {
        return Err(err("spool.max_body must be greater than zero"));
    }
    if cfg.spool.max_memory.get() == 0 {
        return Err(err("spool.max_memory must be greater than zero"));
    }
    if cfg.spool.max_body > cfg.spool.max_memory {
        return Err(err(format!(
            "spool.max_body ({} bytes) exceeds spool.max_memory ({} bytes); \
             not one response could ever be spooled",
            cfg.spool.max_body.get(),
            cfg.spool.max_memory.get()
        )));
    }
    // Spooling a streaming route would hold every chunk until the last one,
    // which is the opposite of what the class is for. Refused rather than
    // ignored: silently not spooling is indistinguishable from spooling.
    for route in &cfg.routes {
        let asked = route.spool.as_ref().and_then(|spool| spool.enabled) == Some(true);
        if asked && route.class == Some(ClassOverride::Streaming) {
            return Err(err(format!(
                "route `{}` is class streaming and sets spool.enabled: true; a spool withholds the \
                 body until the origin finishes, which is exactly what a streaming route must not do",
                route.id
            )));
        }
    }
    Ok(())
}

fn validate_upgrade(cfg: &Config) -> Result<()> {
    check_concurrency_max(cfg.upgrade.max_concurrent, "upgrade.max_concurrent")?;
    if cfg.upgrade.enabled && cfg.upgrade.max_concurrent == 0 {
        return Err(err(
            "upgrade.enabled is true but upgrade.max_concurrent is 0, which admits nothing; set a \
             ceiling or disable upgrades",
        ));
    }
    Ok(())
}

/// The zero-downtime upgrade runs on these three paths agreeing between two
/// processes, so a nonsense value here is only discovered during the restart
/// it was supposed to make safe.
fn validate_graceful(cfg: &Config) -> Result<()> {
    let g = &cfg.server.graceful;
    if g.pid_file.trim().is_empty() {
        return Err(err(
            "server.graceful.pid_file is empty; omit the key to take the default",
        ));
    }
    if g.upgrade_socket.trim().is_empty() {
        return Err(err(
            "server.graceful.upgrade_socket is empty; omit the key to take the default",
        ));
    }
    if g.pid_file == g.upgrade_socket {
        return Err(err(
            "server.graceful.pid_file and server.graceful.upgrade_socket are the same path",
        ));
    }
    if g.shutdown_timeout == super::units::Dur::ZERO {
        return Err(err(
            "server.graceful.shutdown_timeout is 0, which cuts off every in-flight request \
             the moment a shutdown starts; that is the outage a graceful shutdown exists to \
             avoid",
        ));
    }
    Ok(())
}

/// The admin surface publishes backend health, cache occupancy and the config
/// generation. It must not share a listener with anything that serves the
/// public, and it must not be reachable at a path the origin also owns — which
/// is why it is an address rather than a route.
fn validate_admin(cfg: &Config) -> Result<()> {
    let Some(admin) = &cfg.telemetry.admin else {
        return Ok(());
    };
    validate_listen(&admin.listen, "telemetry.admin.listen")?;

    // Compared as parsed addresses, not as strings. Equality alone is not
    // enough: `0.0.0.0:9091` also owns `127.0.0.1:9091`, and `[::]` may be a
    // dual-stack listener depending on the platform.
    let admin_addr = admin.listen.parse::<std::net::SocketAddr>().ok();
    let mut taken: Vec<(&str, &str)> = vec![("server.listen", cfg.server.listen.as_str())];
    if let Some(tls) = &cfg.server.tls {
        taken.push(("server.tls.listen", tls.listen.as_str()));
    }
    if let Some(p) = &cfg.telemetry.prometheus {
        taken.push(("telemetry.prometheus.listen", p.listen.as_str()));
    }
    for (name, address) in taken {
        let taken_addr = address.parse::<std::net::SocketAddr>().ok();
        if admin_addr
            .zip(taken_addr)
            .is_some_and(|(admin, taken)| listeners_overlap(admin, taken))
        {
            return Err(err(format!(
                "telemetry.admin.listen `{}` is the same address as {name}; the admin \
                 endpoints publish backend health, cache occupancy and the configuration \
                 generation, and must not share a listener with traffic",
                admin.listen
            )));
        }
    }
    Ok(())
}

fn listeners_overlap(a: std::net::SocketAddr, b: std::net::SocketAddr) -> bool {
    if a.port() != b.port() {
        return false;
    }
    if a.ip() == b.ip() {
        return true;
    }

    // An IPv6 wildcard commonly accepts IPv4 too unless IPV6_V6ONLY is set;
    // reject that platform-dependent collision conservatively. An IPv4
    // wildcard does not, however, own a specific IPv6 address.
    (a.ip().is_unspecified() && (a.is_ipv6() || b.is_ipv4()))
        || (b.ip().is_unspecified() && (b.is_ipv6() || a.is_ipv4()))
}

fn validate_metrics_export(cfg: &Config) -> Result<()> {
    let Some(otlp) = &cfg.telemetry.metrics.otlp else {
        return Ok(());
    };
    crate::telemetry::transport::parse_endpoint(&otlp.endpoint, "/v1/metrics")
        .map_err(|why| err(format!("telemetry.metrics.otlp.endpoint: {why}")))?;
    crate::telemetry::transport::render_headers(&otlp.headers)
        .map_err(|why| err(format!("telemetry.metrics.otlp.headers: {why}")))?;
    let interval = otlp.interval.as_duration();
    if interval < Duration::from_secs(1) || interval > Duration::from_secs(3600) {
        return Err(err(
            "telemetry.metrics.otlp.interval must be between 1s and 1h",
        ));
    }
    if otlp.timeout == super::units::Dur::ZERO || otlp.timeout.as_duration() >= interval {
        return Err(err(
            "telemetry.metrics.otlp.timeout must be greater than zero and shorter than the              interval, or exports would overlap",
        ));
    }
    for (name, value) in &otlp.labels {
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with("__");
        if !valid {
            return Err(err(format!(
                "telemetry.metrics.otlp.labels: {name:?} is not a valid label name"
            )));
        }
        if crate::telemetry::metrics::LABEL_NAMES.contains(&name.as_str()) {
            return Err(err(format!(
                "telemetry.metrics.otlp.labels: {name} is already a label on Tanod's own metrics"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(err(format!(
                "telemetry.metrics.otlp.labels: the value of {name} contains a control character"
            )));
        }
    }
    Ok(())
}

fn validate_tracing(cfg: &Config) -> Result<()> {
    let t = &cfg.telemetry.tracing;
    if t.service_name.as_ref().is_some_and(|n| n.trim().is_empty()) {
        return Err(err(
            "telemetry.tracing.service_name is empty; omit the key to take `tanod`",
        ));
    }
    if t.sample.one_in == 0 {
        // Ambiguous between "everything" and "nothing", and the two readings
        // differ by the entire contents of a tracing backend.
        return Err(err(
            "telemetry.tracing.sample.one_in is 0, which is ambiguous; use `one_in: 1` for \
             every request or `mode: never` for none",
        ));
    }
    let Some(otlp) = &t.otlp else {
        return Ok(());
    };
    crate::telemetry::otlp::parse_endpoint(&otlp.endpoint)
        .map_err(|why| err(format!("telemetry.tracing.otlp.endpoint: {why}")))?;
    crate::telemetry::transport::render_headers(&otlp.headers)
        .map_err(|why| err(format!("telemetry.tracing.otlp.headers: {why}")))?;
    if otlp.max_queue == 0 || otlp.max_batch == 0 {
        return Err(err(
            "telemetry.tracing.otlp.max_queue and max_batch must be greater than zero",
        ));
    }
    if otlp.max_batch > otlp.max_queue {
        return Err(err(format!(
            "telemetry.tracing.otlp.max_batch ({}) exceeds max_queue ({}); a batch can never \
             fill",
            otlp.max_batch, otlp.max_queue
        )));
    }
    if otlp.timeout == super::units::Dur::ZERO || otlp.interval == super::units::Dur::ZERO {
        return Err(err(
            "telemetry.tracing.otlp.timeout and interval must be greater than zero",
        ));
    }
    Ok(())
}

fn validate_listen(address: &str, path: &str) -> Result<()> {
    address
        .parse::<std::net::SocketAddr>()
        .map(|_| ())
        .map_err(|_| {
            err(format!(
                "{path} `{address}` is not a valid IP socket address"
            ))
        })
}

fn validate_upstream(address: &str) -> Result<()> {
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| err(format!("origin upstream `{address}` has no port")))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(err(format!(
            "origin upstream `{address}` is not a valid host:port"
        )));
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(err(format!(
            "origin upstream `{address}` uses an IPv6 address without brackets"
        )));
    }
    Ok(())
}

/// A waiter that gives up before the work it waits on can finish converts one
/// managed queue into a real stampede — the precise failure Tanod exists to
/// prevent. The wait must cover the origin timeout.
fn check_coalesce_wait(cfg: &Config) -> Result<()> {
    if let Some(wait) = cfg.coalesce.wait_timeout
        && wait < cfg.timeouts.origin
    {
        return Err(err(format!(
            "coalesce.wait_timeout ({:?}) is shorter than timeouts.origin ({:?}); \
             every waiter would be released to the origin before the leader could finish. \
             Omit wait_timeout to track the origin timeout automatically.",
            wait.as_duration(),
            cfg.timeouts.origin.as_duration()
        )));
    }
    Ok(())
}

/// The longest a request may be made to wait for a permit.
///
/// A queue deadline is a wait on the request path, so an hour is already far
/// past anything an origin governor should allow; the bound exists to catch a
/// wrong unit (`timeout: 30m` where `30s` was meant) and to keep the value out
/// of the range where `Instant::now() + timeout` stops being representable.
const MAX_QUEUE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

fn check_concurrency_max(value: usize, path: &str) -> Result<()> {
    if value > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(err(format!(
            "{path} is {value}, greater than Tokio's semaphore maximum of {}",
            tokio::sync::Semaphore::MAX_PERMITS
        )));
    }
    Ok(())
}

fn check_queue(c: &Concurrency, path: &str) -> Result<()> {
    if c.queue.max > 0 && c.queue.timeout == super::units::Dur::ZERO {
        return Err(err(format!(
            "{path}.queue.max is {} but queue.timeout is 0; a queue with no deadline is unbounded in \
             time",
            c.queue.max
        )));
    }
    if c.queue.timeout.as_duration() > MAX_QUEUE_TIMEOUT {
        return Err(err(format!(
            "{path}.queue.timeout is {:?}, longer than the {:?} maximum; a request waiting \
             that long for a permit has already failed somewhere else",
            c.queue.timeout.as_duration(),
            MAX_QUEUE_TIMEOUT
        )));
    }
    Ok(())
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::High => "high",
        Priority::Normal => "normal",
        Priority::Low => "low",
    }
}

/// The shortest purge token worth having.
///
/// Not arbitrary: the endpoint is reachable by anyone who can reach the admin
/// listener, and a short shared secret on an invalidation endpoint is a
/// stampede trigger behind a lock anybody can pick. Long enough that guessing
/// is not the attack, short enough to paste.
const MIN_PURGE_TOKEN: usize = 24;

fn validate_purge(cfg: &Config) -> Result<()> {
    let Some(token) = cfg.cache.purge.token.as_deref() else {
        return Ok(());
    };
    if cfg.telemetry.admin.is_none() {
        return Err(err(
            "cache.purge.token is set but telemetry.admin is not configured; the purge endpoint              is served on the admin listener, so there is nothing listening for it",
        ));
    }
    if token.len() < MIN_PURGE_TOKEN {
        return Err(err(format!(
            "cache.purge.token is {} characters; at least {MIN_PURGE_TOKEN} are required. The              purge endpoint makes an origin re-render on demand, so a guessable secret in front              of it is a stampede anybody can trigger",
            token.len()
        )));
    }
    // A token has to survive a shell, a Kubernetes secret and an
    // `Authorization` header unchanged. Refusing the awkward bytes up front
    // beats an operator debugging why a correct-looking token is rejected.
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(err(
            "cache.purge.token must be printable ASCII with no spaces; it travels in an              Authorization header",
        ));
    }
    Ok(())
}

fn validate_tag_header(cfg: &Config) -> Result<()> {
    let name = &cfg.cache.tag_header;
    if http::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
        return Err(err(format!(
            "cache.tag_header `{name}` is not a valid HTTP header name"
        )));
    }
    // Reading tags from a header the client controls would let anyone tag a
    // response into somebody else's invalidation group — or, with a purge
    // token in hand, decide what a purge destroys. Tags come from the origin.
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "cookie" | "authorization" | "host" | "accept" | "accept-encoding" | "user-agent"
    ) {
        return Err(err(format!(
            "cache.tag_header is `{name}`, which is a request header; invalidation tags are              declared by the origin on its response"
        )));
    }
    Ok(())
}

/// A breaker that cannot eject anything is a protection somebody believes they
/// turned on. Every check here exists because the setting parses cleanly while
/// describing something inert or self-defeating.
fn validate_breaker(cfg: &Config) -> Result<()> {
    let b = &cfg.origin.breaker;
    if !b.enabled {
        return Ok(());
    }
    if cfg.origin.upstreams.len() < 2 {
        return Err(err(format!(
            "origin.breaker.enabled is true with {} upstream; ejecting the only backend would \
             turn a partial failure into a total outage, so the ejection cap keeps it in \
             rotation and the breaker never does anything. Add a backend, or remove the block",
            cfg.origin.upstreams.len()
        )));
    }
    if b.window == super::units::Dur::ZERO || b.open_for == super::units::Dur::ZERO {
        return Err(err(
            "origin.breaker.window and origin.breaker.open_for must be greater than zero",
        ));
    }
    if b.min_requests == 0 {
        return Err(err(
            "origin.breaker.min_requests is 0; a backend would be ejected on its first failure, \
             which is noise rather than a signal",
        ));
    }
    if !(1..=100).contains(&b.failure_percent) {
        return Err(err(format!(
            "origin.breaker.failure_percent is {}; it is a percentage of the requests in the \
             window and must be between 1 and 100",
            b.failure_percent
        )));
    }
    if b.max_ejected_percent > 100 {
        return Err(err(format!(
            "origin.breaker.max_ejected_percent is {}; it is a percentage of the pool and cannot \
             exceed 100",
            b.max_ejected_percent
        )));
    }
    // `len * percent / 100` floored. Zero means the first ejection already
    // exceeds the cap, so breakers are consulted and then ignored forever.
    if cfg.origin.upstreams.len() * b.max_ejected_percent as usize / 100 == 0 {
        return Err(err(format!(
            "origin.breaker.max_ejected_percent is {} across {} upstreams, which allows 0 \
             backends to be ejected at once; the breaker would observe failures and never act \
             on them. Raise it to at least {}%",
            b.max_ejected_percent,
            cfg.origin.upstreams.len(),
            100_usize.div_ceil(cfg.origin.upstreams.len())
        )));
    }
    Ok(())
}

fn validate_retry(cfg: &Config) -> Result<()> {
    let r = &cfg.origin.retry;
    if !r.enabled {
        return Ok(());
    }
    if r.max_attempts < 2 {
        return Err(err(format!(
            "origin.retry.enabled is true but max_attempts is {}; attempts include the first \
             try, so anything below 2 never retries",
            r.max_attempts
        )));
    }
    if r.window == super::units::Dur::ZERO {
        return Err(err(
            "origin.retry.window must be greater than zero; the budget is measured over it",
        ));
    }
    if r.budget_percent > 100 {
        return Err(err(format!(
            "origin.retry.budget_percent is {}; a budget above 100% would let retries outnumber \
             the requests that caused them, which is the amplification the budget exists to \
             prevent",
            r.budget_percent
        )));
    }
    if r.budget_percent == 0 && r.budget_min == 0 {
        return Err(err(
            "origin.retry.enabled is true but both budget_percent and budget_min are 0, so no \
             retry can ever be afforded",
        ));
    }
    Ok(())
}

fn validate_priorities(cfg: &Config) -> Result<()> {
    let p = &cfg.origin.priorities;
    let max = cfg.origin.concurrency.max;
    for (name, percent) in [("high", p.high), ("normal", p.normal), ("low", p.low)] {
        if !(1..=100).contains(&percent) {
            return Err(err(format!(
                "origin.priorities.{name} is {percent}; it is a percentage of \
                 origin.concurrency.max and must be between 1 and 100"
            )));
        }
        if max.saturating_mul(percent as usize) / 100 == 0 {
            return Err(err(format!(
                "origin.priorities.{name} is {percent}% of an origin.concurrency.max of {max}, \
                 which rounds to a ceiling of 0 and would refuse every request at that priority. \
                 Raise the share to at least {}%, or raise concurrency.max",
                100_usize.div_ceil(max.max(1))
            )));
        }
    }
    Ok(())
}

/// A weight larger than a ceiling it is charged against can never be
/// satisfied: the request waits out its queue deadline and is shed, every
/// time, on a route that looks correctly configured.
fn check_route_weight(route: &Route, cfg: &Config) -> Result<()> {
    let id = &route.id;
    let weight = route.weight;
    if weight == 0 {
        return Err(err(format!(
            "route `{id}`: weight is 0, which would take no capacity and bound nothing; omit the \
             key to take the default of 1"
        )));
    }
    let weight = weight as usize;
    let global = cfg.origin.concurrency.max;
    if weight > global {
        return Err(err(format!(
            "route `{id}`: weight {weight} exceeds origin.concurrency.max ({global}), so no \
             request on this route could ever be admitted"
        )));
    }
    let percent = cfg.origin.priorities.percent_for(route.priority);
    let tier = (global.saturating_mul(percent as usize) / 100).max(1);
    if weight > tier {
        return Err(err(format!(
            "route `{id}`: weight {weight} exceeds the ceiling of its `{}` priority tier \
             ({tier} = {percent}% of origin.concurrency.max), so no request on this route could \
             ever be admitted",
            priority_name(route.priority)
        )));
    }
    if let Some(c) = &route.concurrency
        && weight > c.max
    {
        return Err(err(format!(
            "route `{id}`: weight {weight} exceeds its own concurrency.max ({}), so no request \
             on this route could ever be admitted",
            c.max
        )));
    }
    Ok(())
}

fn check_route(route: &Route, cfg: &Config) -> Result<()> {
    let id = &route.id;
    let is_private = route.class == Some(ClassOverride::PrivateDynamic);

    check_route_weight(route, cfg)?;
    if route.priority != Priority::Normal && cfg.origin.priorities.is_uniform() {
        return Err(err(format!(
            "route `{id}` sets priority: {} but origin.priorities leaves every tier at 100%, so \
             every priority competes for the same ceiling and the setting does nothing. Lower \
             the share of the tiers this route should outrank — for example \
             `origin.priorities: {{ low: 50 }}` to keep half the origin ceiling away from \
             low-priority work",
            priority_name(route.priority)
        )));
    }

    if let Some(c) = &route.concurrency {
        if c.max == 0 {
            return Err(err(format!(
                "route `{id}`: concurrency.max is 0, which admits nothing"
            )));
        }
        check_concurrency_max(c.max, &format!("route `{id}`.concurrency.max"))?;
        check_queue(c, &format!("route `{id}`.concurrency"))?;
    }

    if let Some(cache) = &route.cache {
        if is_private && cache.enabled == Some(true) {
            return Err(err(format!(
                "route `{id}` is class private_dynamic but sets cache.enabled: true; \
                 per-user responses are never response-cached"
            )));
        }
        if cache.override_origin {
            if is_private {
                return Err(err(format!(
                    "route `{id}` is class private_dynamic and sets cache.override_origin; \
                     overriding the origin on a private route is how one user's page reaches another"
                )));
            }
            if route.class.is_none() {
                return Err(err(format!(
                    "route `{id}` sets cache.override_origin without declaring a class; \
                     add `class: public_ssr` to state that this route's responses are shareable"
                )));
            }
            if cache.ttl.as_ref().and_then(|t| t.max).is_none() {
                return Err(err(format!(
                    "route `{id}` sets cache.override_origin but no cache.ttl.max; \
                     an override with no ceiling has no bound on how stale a shared response can get"
                )));
            }
        }
        if let Some(v) = &cache.vary {
            for h in &v.headers {
                let lower = h.to_ascii_lowercase();
                if matches!(lower.as_str(), "cookie" | "authorization") {
                    return Err(err(format!(
                        "route `{id}`: cache.vary lists `{h}`; varying on a credential header \
                         creates one cache entry per user and is never what you want"
                    )));
                }
                if lower == "user-agent" || lower == "*" {
                    return Err(err(format!(
                        "route `{id}`: cache.vary lists `{h}`, which has effectively unbounded \
                         cardinality"
                    )));
                }
            }
        }
        if let Some(q) = &cache.query
            && q.keys.is_empty()
        {
            return Err(err(format!(
                "route `{id}`: cache.query.mode is set but cache.query.keys is empty"
            )));
        }
    }

    if let Some(co) = &route.coalesce
        && co.override_origin
        && is_private
    {
        return Err(err(format!(
            "route `{id}` is class private_dynamic and sets coalesce.override_origin; \
             per-user responses are never shared between requests"
        )));
    }
    if route
        .coalesce
        .as_ref()
        .is_some_and(|co| co.override_origin && co.enabled == Some(false))
    {
        return Err(err(format!(
            "route `{id}` sets coalesce.override_origin while coalescing is disabled"
        )));
    }

    if let Some(vary) = route.cache.as_ref().and_then(|cache| cache.vary.as_ref()) {
        let mut seen = HashSet::new();
        for name in &vary.headers {
            let parsed = http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                err(format!(
                    "route `{id}`: cache.vary header `{name}` is invalid"
                ))
            })?;
            if !seen.insert(parsed) {
                return Err(err(format!(
                    "route `{id}`: cache.vary repeats header `{name}`"
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config as _Config;

    fn parse(yaml: &str) -> _Config {
        serde_saphyr::from_str(yaml).expect("yaml should parse")
    }

    const BASE: &str = r#"
version: 1
origin:
  upstreams: ["next-1:3000"]
"#;

    #[test]
    fn accepts_a_minimal_config() {
        validate(&parse(BASE)).unwrap();
    }

    #[test]
    fn accepts_a_supervised_origin_on_loopback() {
        let cfg = parse(
            "version: 1\norigin:\n  upstreams: [\"127.0.0.1:3001\"]\n  command:\n    args: [node, server.js]\n    env: {NODE_ENV: production}\n",
        );
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_a_supervised_origin_that_cannot_be_the_upstream() {
        for (origin, expected) in [
            (
                "upstreams: [\"web:3000\"]\n  command: {args: [node, s.js]}",
                "loopback",
            ),
            (
                "upstreams: [\"127.0.0.1:3001\", \"127.0.0.1:3002\"]\n  command: {args: [node, s.js]}",
                "exactly one upstream",
            ),
            (
                "upstreams: [\"127.0.0.1:3001\"]\n  command: {args: []}",
                "args is empty",
            ),
            (
                "upstreams: [\"127.0.0.1:3001\"]\n  command: {args: [node], env: {\"BAD-NAME\": x}}",
                "environment variable name",
            ),
            (
                "upstreams: [\"127.0.0.1:3001\"]\n  command: {args: [node], ready_timeout: 500ms}",
                "ready_timeout",
            ),
        ] {
            let cfg = parse(&format!("version: 1\norigin:\n  {origin}\n"));
            let error = validate(&cfg).unwrap_err();
            assert!(error.0.contains(expected), "{origin}: {error}");
        }
    }

    #[test]
    fn accepts_a_customised_overload_page() {
        let cfg = parse(&format!(
            "{BASE}overload:\n  page:\n    title: \"Sandali lang\"\n    message: \"Babalik ka sa loob ng {{{{refresh}}}} segundo.\"\n    lang: fil-PH\n    refresh: 10s\n"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_an_overload_page_that_would_misbehave() {
        for (page, expected) in [
            ("refresh: 500ms", "refresh"),
            ("refresh: 11m", "refresh"),
            ("lang: \"en\\\" onload=x\"", "language tag"),
            ("lang: \"\"", "language tag"),
            ("title: \"  \"", "title"),
        ] {
            let cfg = parse(&format!("{BASE}overload:\n  page:\n    {page}\n"));
            let error = validate(&cfg).unwrap_err();
            assert!(error.0.contains(expected), "{page}: {error}");
        }
    }

    #[test]
    fn rejects_an_oversized_overload_page_file() {
        let mut cfg = parse(&format!("{BASE}overload:\n  page:\n    file: busy.html\n"));
        cfg.overload.page.template = Some("x".repeat(MAX_OVERLOAD_PAGE_BYTES + 1));
        let error = validate(&cfg).unwrap_err();
        assert!(
            error.0.contains("busy.html is larger than 64 KiB"),
            "{error}"
        );
        cfg.overload.page.template = Some("x".repeat(MAX_OVERLOAD_PAGE_BYTES));
        validate(&cfg).unwrap();
    }

    #[test]
    fn accepts_a_conservative_static_replica_partition() {
        let cfg = parse(&format!(
            "{BASE}  concurrency:\n    max: 4\ncapacity:\n  global_max: 9\n  replicas: 2\n"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_a_replica_partition_that_multiplies_the_global_budget() {
        let cfg = parse(&format!(
            "{BASE}  concurrency:\n    max: 5\ncapacity:\n  global_max: 8\n  replicas: 2\n"
        ));
        let error = validate(&cfg).unwrap_err();
        assert!(error.0.contains("allocates 10"), "{error}");
        assert!(error.0.contains("global_max (8)"), "{error}");
    }

    #[test]
    fn rejects_an_empty_replica_capacity_contract() {
        for capacity in [
            "capacity:\n  global_max: 0\n  replicas: 2\n",
            "capacity:\n  global_max: 8\n  replicas: 0\n",
        ] {
            assert!(validate(&parse(&format!("{BASE}{capacity}"))).is_err());
        }
    }

    // ------------------------------------------------- cache lifecycle

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const ADMIN: &str = "telemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n";

    #[test]
    fn accepts_a_purge_token_alongside_an_admin_listener() {
        validate(&parse(&format!(
            "{BASE}cache:\n  purge:\n    token: \"{TOKEN}\"\n{ADMIN}"
        )))
        .unwrap();
    }

    /// The endpoint is served on the admin listener, so a token without one is
    /// a protection configured against nothing.
    #[test]
    fn a_purge_token_without_an_admin_listener_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}cache:\n  purge:\n    token: \"{TOKEN}\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("telemetry.admin"), "{e}");
    }

    #[test]
    fn a_short_purge_token_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}cache:\n  purge:\n    token: \"short\"\n{ADMIN}"
        )))
        .unwrap_err();
        assert!(e.0.contains("at least 24"), "{e}");
        assert!(e.0.contains("stampede"), "names the consequence: {e}");
    }

    #[test]
    fn a_purge_token_that_cannot_travel_in_a_header_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}cache:\n  purge:\n    token: \"has a space in it and is long enough\"\n{ADMIN}"
        )))
        .unwrap_err();
        assert!(e.0.contains("printable ASCII"), "{e}");
    }

    #[test]
    fn accepts_the_next_js_cache_tag_header() {
        validate(&parse(&format!(
            "{BASE}cache:\n  tag_header: \"x-next-cache-tags\"\n"
        )))
        .unwrap();
    }

    /// Tags decide what a purge destroys. Taking them from a header the client
    /// controls would hand that decision to the client.
    #[test]
    fn a_request_header_as_the_tag_source_is_refused() {
        for header in ["Cookie", "authorization", "User-Agent", "Accept"] {
            let e = validate(&parse(&format!(
                "{BASE}cache:\n  tag_header: \"{header}\"\n"
            )))
            .unwrap_err();
            assert!(e.0.contains("declared by the origin"), "{e}");
        }
    }

    #[test]
    fn a_malformed_tag_header_name_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}cache:\n  tag_header: \"not a header\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("valid HTTP header name"), "{e}");
    }

    #[test]
    fn both_eviction_policies_are_accepted() {
        for policy in ["clock", "fifo"] {
            validate(&parse(&format!("{BASE}cache:\n  eviction: {policy}\n"))).unwrap();
        }
    }

    // ------------------------------------------------- origin resilience

    /// Two upstreams, so a breaker has somewhere to eject to.
    const PAIR: &str = r#"
version: 1
origin:
  upstreams: ["next-1:3000", "next-2:3000"]
"#;

    #[test]
    fn accepts_a_configured_breaker_retry_budget_and_priorities() {
        validate(&parse(&format!(
            "{PAIR}  breaker:\n    enabled: true\n  retry:\n    enabled: true\n  \
             priorities:\n    low: 50\n"
        )))
        .unwrap();
    }

    #[test]
    fn accepts_least_loaded_selection() {
        validate(&parse(&format!("{PAIR}  load_balancing: least_loaded\n"))).unwrap();
    }

    /// The ejection cap keeps the only backend in rotation, so the breaker
    /// would observe failures and never act on one.
    #[test]
    fn a_breaker_with_one_upstream_is_refused_rather_than_left_inert() {
        let e = validate(&parse(&format!("{BASE}  breaker:\n    enabled: true\n"))).unwrap_err();
        assert!(e.0.contains("total outage"), "{e}");
    }

    #[test]
    fn an_ejection_cap_that_rounds_to_no_backends_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  breaker:\n    enabled: true\n    max_ejected_percent: 10\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("allows 0"), "{e}");
        assert!(
            e.0.contains("50%"),
            "names the smallest cap that works: {e}"
        );
    }

    #[test]
    fn a_failure_threshold_outside_a_percentage_is_refused() {
        for percent in ["0", "101"] {
            let e = validate(&parse(&format!(
                "{PAIR}  breaker:\n    enabled: true\n    failure_percent: {percent}\n"
            )))
            .unwrap_err();
            assert!(e.0.contains("failure_percent"), "{e}");
        }
    }

    #[test]
    fn a_breaker_that_trips_on_the_first_failure_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  breaker:\n    enabled: true\n    min_requests: 0\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("noise"), "{e}");
    }

    #[test]
    fn a_retry_setting_that_can_never_retry_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  retry:\n    enabled: true\n    max_attempts: 1\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("never retries"), "{e}");
    }

    #[test]
    fn a_retry_budget_above_one_hundred_percent_is_refused() {
        // A budget that allows more retries than there were requests is the
        // amplification the budget exists to prevent.
        let e = validate(&parse(&format!(
            "{PAIR}  retry:\n    enabled: true\n    budget_percent: 150\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("amplification"), "{e}");
    }

    #[test]
    fn a_retry_budget_of_nothing_at_all_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  retry:\n    enabled: true\n    budget_percent: 0\n    budget_min: 0\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("no retry can ever be afforded"), "{e}");
    }

    #[test]
    fn a_priority_share_that_rounds_to_no_capacity_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  concurrency:\n    max: 5\n  priorities:\n    low: 10\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("refuse every request"), "{e}");
        assert!(e.0.contains("20%"), "names a share that would work: {e}");
    }

    #[test]
    fn a_priority_share_outside_a_percentage_is_refused() {
        let e = validate(&parse(&format!("{PAIR}  priorities:\n    high: 0\n"))).unwrap_err();
        assert!(e.0.contains("priorities.high"), "{e}");
    }

    /// The accepted-and-ignored failure this project refuses everywhere else:
    /// routes labelled by priority while every tier may use the whole ceiling.
    #[test]
    fn a_route_priority_with_no_tier_shares_set_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}routes:\n  - id: reports\n    match: \"/reports/**\"\n    priority: low\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("origin.priorities"), "{e}");
        assert!(e.0.contains("does nothing"), "{e}");
    }

    #[test]
    fn a_route_priority_is_accepted_once_the_tiers_differ() {
        validate(&parse(&format!(
            "{PAIR}  priorities:\n    low: 50\nroutes:\n  - id: reports\n    \
             match: \"/reports/**\"\n    priority: low\n"
        )))
        .unwrap();
    }

    #[test]
    fn a_weight_larger_than_the_global_ceiling_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  concurrency:\n    max: 4\nroutes:\n  - id: search\n    \
             match: \"/search\"\n    weight: 5\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("origin.concurrency.max (4)"), "{e}");
        assert!(e.0.contains("could ever be admitted"), "{e}");
    }

    #[test]
    fn a_weight_larger_than_its_own_route_ceiling_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}routes:\n  - id: search\n    match: \"/search\"\n    weight: 5\n    \
             concurrency:\n      max: 4\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("concurrency.max (4)"), "{e}");
    }

    #[test]
    fn a_weight_larger_than_its_priority_tier_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}  concurrency:\n    max: 10\n  priorities:\n    low: 20\nroutes:\n  \
             - id: reports\n    match: \"/reports/**\"\n    priority: low\n    weight: 3\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("`low` priority tier"), "{e}");
        assert!(e.0.contains("(2 = 20%"), "{e}");
    }

    #[test]
    fn a_zero_weight_is_refused() {
        let e = validate(&parse(&format!(
            "{PAIR}routes:\n  - id: search\n    match: \"/search\"\n    weight: 0\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("bound nothing"), "{e}");
    }

    // ------------------------------------------------- schema versioning

    #[test]
    fn an_unknown_schema_version_is_refused_and_names_both_numbers() {
        // Coercing a future version would mean applying a policy nobody
        // wrote, because a later release may reinterpret a key that exists
        // today.
        let e = validate(&parse(BASE.replace("version: 1", "version: 2").as_str())).unwrap_err();
        assert!(e.0.contains('2') && e.0.contains("version 1"), "{e}");
        assert!(e.0.contains("CONFIG-SCHEMA"), "{e}");
        assert!(validate(&parse(BASE.replace("version: 1", "version: 0").as_str())).is_err());
    }

    // ------------------------------------------------- admin endpoints

    #[test]
    fn the_admin_listener_is_accepted_on_its_own_address() {
        validate(&parse(&format!(
            "{BASE}telemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n"
        )))
        .unwrap();
    }

    #[test]
    fn the_admin_listener_may_not_share_an_address_with_traffic() {
        // It publishes backend health, cache occupancy and the config
        // generation. Sharing the traffic listener would publish all three to
        // anyone who can reach the site.
        let e = validate(&parse(&format!(
            "{BASE}server:\n  listen: \"127.0.0.1:8080\"\ntelemetry:\n  admin:\n    listen: \"127.0.0.1:8080\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("server.listen"), "{e}");
    }

    #[test]
    fn the_admin_listener_may_not_share_an_address_with_metrics() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  prometheus:\n    listen: \"127.0.0.1:9090\"\n  admin:\n    listen: \"127.0.0.1:9090\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("telemetry.prometheus.listen"), "{e}");
    }

    #[test]
    fn the_admin_listener_may_not_hide_under_a_wildcard_traffic_listener() {
        let e = validate(&parse(&format!(
            "{BASE}server:\n  listen: \"0.0.0.0:9091\"\ntelemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("server.listen"), "{e}");
    }

    #[test]
    fn a_wildcard_admin_listener_may_not_cover_a_specific_metrics_listener() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  prometheus:\n    listen: \"127.0.0.1:9090\"\n  admin:\n    listen: \"0.0.0.0:9090\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("telemetry.prometheus.listen"), "{e}");
    }

    #[test]
    fn an_ipv4_wildcard_does_not_claim_a_specific_ipv6_address() {
        let cfg = parse(&format!(
            "{BASE}server:\n  listen: \"0.0.0.0:9091\"\ntelemetry:\n  admin:\n    listen: \"[::1]:9091\"\n"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn an_ipv6_wildcard_is_conservatively_treated_as_dual_stack() {
        let e = validate(&parse(&format!(
            "{BASE}server:\n  listen: \"[::]:9091\"\ntelemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("server.listen"), "{e}");
    }

    #[test]
    fn strict_upstream_readiness_requires_an_active_health_check() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n    require_healthy_upstream: true\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("no health check"), "{e}");
    }

    #[test]
    fn a_malformed_admin_address_is_refused() {
        assert!(
            validate(&parse(&format!(
                "{BASE}telemetry:\n  admin:\n    listen: \"not-an-address\"\n"
            )))
            .is_err()
        );
    }

    // ------------------------------------------------- tracing

    #[test]
    fn a_tracing_block_without_export_is_accepted() {
        // Correlation is unconditional; export is the optional half.
        validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    sample:\n      mode: ratio\n      one_in: 20\n"
        )))
        .unwrap();
    }

    #[test]
    fn an_https_otlp_endpoint_needs_the_tls_feature_rather_than_being_downgraded() {
        for block in ["tracing", "metrics"] {
            let result = validate(&parse(&format!(
                "{BASE}telemetry:\n  {block}:\n    otlp:\n      endpoint: \"https://collector/v1/x\"\n"
            )));
            if cfg!(feature = "tls") {
                result.unwrap();
            } else {
                let e = result.unwrap_err();
                assert!(e.0.contains("tls"), "{e}");
            }
        }
    }

    #[test]
    fn metric_export_settings_are_checked() {
        for (otlp, expected) in [
            (
                "endpoint: \"http://c/v1/metrics\"\n      interval: 500ms",
                "interval",
            ),
            (
                "endpoint: \"http://c/v1/metrics\"\n      timeout: 60s",
                "timeout",
            ),
            (
                "endpoint: \"http://c/v1/metrics\"\n      labels: {route: x}",
                "already a label",
            ),
            (
                "endpoint: \"http://c/v1/metrics\"\n      labels: {\"bad-name\": x}",
                "label name",
            ),
            (
                "endpoint: \"http://c/v1/metrics\"\n      headers: {Host: x}",
                "set by Tanod",
            ),
            ("endpoint: \"ftp://c\"", "endpoint"),
        ] {
            let e = validate(&parse(&format!(
                "{BASE}telemetry:\n  metrics:\n    otlp:\n      {otlp}\n"
            )))
            .unwrap_err();
            assert!(e.0.contains(expected), "{otlp}: {e}");
        }
        validate(&parse(&format!(
            "{BASE}telemetry:\n  metrics:\n    otlp:\n      endpoint: \"http://c/v1/metrics\"\n      labels: {{environment: staging}}\n"
        )))
        .unwrap();
    }

    #[test]
    fn an_ambiguous_sample_ratio_is_refused() {
        // `one_in: 0` reads equally as "everything" and "nothing", and the two
        // differ by the entire contents of a tracing backend.
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    sample:\n      one_in: 0\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("mode: never"), "{e}");
    }

    #[test]
    fn a_batch_larger_than_the_queue_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    otlp:\n      endpoint: \"http://c:4318/v1/traces\"\n      max_queue: 10\n      max_batch: 100\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("can never"), "{e}");
    }

    // ------------------------------------------------- graceful upgrade

    #[test]
    fn a_zero_shutdown_timeout_is_refused() {
        // It would cut off every in-flight request the instant a shutdown
        // begins, which is the outage graceful shutdown exists to avoid.
        let e = validate(&parse(&format!(
            "{BASE}server:\n  graceful:\n    shutdown_timeout: 0s\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("shutdown_timeout"), "{e}");
    }

    #[test]
    fn the_pid_file_and_upgrade_socket_may_not_be_the_same_path() {
        let e = validate(&parse(&format!(
            "{BASE}server:\n  graceful:\n    pid_file: /run/h\n    upgrade_socket: /run/h\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("same path"), "{e}");
    }

    #[test]
    fn rejects_unknown_keys() {
        // A typo'd key is a silent policy change; serde must refuse it.
        let e = serde_saphyr::from_str::<_Config>(
            r#"
version: 1
origin:
  upstreams: ["a:3000"]
cache:
  enabled: true
  ttl_max: 2s
"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("unknown field"), "{e}");
    }

    #[test]
    fn rejects_an_unparseable_trusted_proxy_block() {
        // A trust list that does not parse is a trust list that protects
        // nothing, and the symptom looks exactly like a working deployment.
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    from: [\"10.0.0.0/33\"]
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("trusted_proxies"), "{e}");
    }

    #[test]
    fn accepts_ipv4_ipv6_and_bare_addresses_in_the_trust_list() {
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    from: [\"10.0.0.0/8\", \"2001:db8::/32\", \"127.0.0.1\"]
    client_ip: forwarded
    scheme: forwarded
"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_a_forwarded_source_with_nobody_to_believe() {
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    client_ip: forwarded
    scheme: forwarded
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("`from` is empty"), "{e}");
    }

    #[test]
    fn the_default_trust_policy_is_accepted_and_believes_nobody() {
        // The default is not "unset and therefore an error": it is a real
        // policy — trust no peer — and it must validate.
        let cfg = parse(BASE);
        validate(&cfg).unwrap();
        assert!(cfg.server.trusted_proxies.from.is_empty());
        assert_eq!(cfg.telemetry.tracing.trust_incoming, TrustIncoming::Never);
    }

    #[test]
    fn rejects_alpn_negotiation_over_cleartext() {
        // There is no ALPN outside a TLS handshake, so `auto` over cleartext
        // is a request that cannot be honoured. Refused here rather than as a
        // connection error on every request.
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  http_version: auto
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("requires origin.tls"), "{e}");
    }

    #[test]
    fn accepts_prior_knowledge_h2c_to_the_origin() {
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  http_version: http2
",
        );
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_tls_when_the_binary_cannot_serve_it() {
        // The failure being prevented: a valid config, a running process, and
        // a TLS port that refuses connections with nothing in the logs.
        let cfg = parse(&format!(
            "{BASE}
server:
  tls:
    listen: \"0.0.0.0:8443\"
    cert: /nonexistent/fullchain.pem
    key: /nonexistent/privkey.pem
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("does not exist"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[cfg(feature = "tls")]
    #[test]
    fn rejects_existing_but_invalid_server_tls_files() {
        let base = std::env::temp_dir().join(format!("tanod-invalid-tls-{}", std::process::id()));
        let cert = base.with_extension("cert.pem");
        let key = base.with_extension("key.pem");
        std::fs::write(&cert, b"this is not a certificate").unwrap();
        std::fs::write(&key, b"this is not a private key").unwrap();

        let cfg = parse(&format!(
            "{BASE}\nserver:\n  tls:\n    listen: \"0.0.0.0:8443\"\n    cert: {}\n    key: {}\n",
            cert.display(),
            key.display()
        ));
        let error = validate(&cfg).unwrap_err().to_string();

        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
        assert!(
            error.contains("no usable certificate/private-key pair")
                || error.contains("could not read server TLS"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_origin_ca_that_the_connector_would_ignore() {
        // The greengate rule: a key that is accepted and then ignored lets
        // someone ship believing a protection is on. Here the belief would be
        // "the origin's certificate is checked against my CA".
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  tls:
    sni: origin.internal
    ca: /etc/tanod/ca.pem
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("origin.tls.ca is not implemented"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[test]
    fn rejects_hostname_verification_without_chain_verification() {
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  tls:
    sni: origin.internal
    verify_cert: false
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("verify_hostname without verify_cert"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[test]
    fn rejects_a_spool_ceiling_that_can_never_be_met() {
        let cfg = parse(&format!(
            "{BASE}
spool:
  enabled: true
  max_body: 8MiB
  max_memory: 4MiB
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("exceeds spool.max_memory"), "{e}");
    }

    #[test]
    fn rejects_spooling_a_streaming_route() {
        // A spool withholds the body until the origin finishes, which is the
        // one thing a streaming route must not do. Refused rather than
        // ignored: silently not spooling is indistinguishable from spooling.
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: feed
    match: \"/feed\"
    class: streaming
    spool:
      enabled: true
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("class streaming"), "{e}");
    }

    #[test]
    fn rejects_an_upgrade_ceiling_that_admits_nothing() {
        let cfg = parse(&format!(
            "{BASE}
upgrade:
  enabled: true
  max_concurrent: 0
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("admits nothing"), "{e}");
    }

    #[test]
    fn rejects_concurrency_values_above_tokios_semaphore_maximum() {
        let too_many = tokio::sync::Semaphore::MAX_PERMITS + 1;
        for (yaml, path) in [
            (
                format!(
                    "version: 1\norigin:\n  upstreams: [\"next-1:3000\"]\n  concurrency:\n    max: {too_many}\n"
                ),
                "origin.concurrency.max",
            ),
            (
                format!("{BASE}upgrade:\n  max_concurrent: {too_many}\n"),
                "upgrade.max_concurrent",
            ),
            (
                format!(
                    "{BASE}routes:\n  - id: large\n    match: /large\n    concurrency:\n      max: {too_many}\n"
                ),
                "route `large`.concurrency.max",
            ),
        ] {
            let error = validate(&parse(&yaml)).unwrap_err().to_string();
            assert!(error.contains(path), "expected {path} in: {error}");
            assert!(error.contains("semaphore maximum"), "{error}");
        }
    }

    #[test]
    fn rejects_coalesce_wait_shorter_than_origin_timeout() {
        let cfg = parse(&format!(
            "{BASE}
timeouts:
  origin: 30s
coalesce:
  wait_timeout: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(
            e.to_string().contains("shorter than timeouts.origin"),
            "{e}"
        );
    }

    #[test]
    fn rejects_cache_override_on_private_route() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: account
    match: \"/account/**\"
    class: private_dynamic
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("private_dynamic"), "{e}");
    }

    #[test]
    fn rejects_cache_override_without_declared_class() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("without declaring a class"), "{e}");
    }

    #[test]
    fn rejects_cache_override_without_ttl_ceiling() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    class: public_ssr
    cache:
      override_origin: true
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("no cache.ttl.max"), "{e}");
    }

    #[test]
    fn accepts_a_properly_fenced_override() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_varying_on_a_credential_header() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    cache:
      vary:
        headers: [\"Cookie\"]
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("credential header")
        );
    }

    #[test]
    fn rejects_queue_without_deadline() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 0s
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("no deadline")
        );
    }

    #[test]
    fn rejects_a_queue_deadline_longer_than_the_maximum() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 2h
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("longer than the")
        );
    }

    /// The bound exists so that no accepted config can reach
    /// `Instant::now() + timeout` with a value that is not representable.
    #[test]
    fn the_longest_accepted_queue_deadline_is_a_representable_instant() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 60m
"
        ));
        validate(&cfg).expect("an hour is on the accepted side of the bound");
        assert!(
            tokio::time::Instant::now()
                .into_std()
                .checked_add(MAX_QUEUE_TIMEOUT)
                .is_some()
        );
    }

    #[test]
    fn rejects_config_that_would_silently_do_nothing() {
        // Each of these parses happily and has no effect, which is how someone
        // ships believing a protection is enabled.
        for (yaml, want) in [
            ("cache:\n  respect_origin: false\n", "respect_origin"),
            (
                "deployment:\n  id_header: \"X-Deployment-ID\"\n",
                "id_header",
            ),
        ] {
            let cfg = parse(&format!("{BASE}{yaml}"));
            let e = validate(&cfg).unwrap_err().to_string();
            assert!(e.contains(want), "expected {want} to be rejected, got: {e}");
            assert!(e.contains("not implemented"), "{e}");
        }
    }

    #[test]
    fn accepts_respect_origin_true_because_that_is_the_behaviour() {
        validate(&parse(&format!("{BASE}cache:\n  respect_origin: true\n"))).unwrap();
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: dup
    match: \"/a\"
  - id: dup
    match: \"/b\"
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("duplicate route id")
        );
    }

    #[test]
    fn rejects_a_body_limit_larger_than_the_cache_budget() {
        let cfg = parse(&format!(
            "{BASE}cache:\n  max_memory: 1KiB\n  max_body_size: 2KiB\n"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("exceeds cache.max_memory")
        );
    }

    #[test]
    fn rejects_invalid_listeners_and_upstreams_before_pingora_can_panic() {
        let cfg = parse(
            "version: 1\nserver:\n  listen: nope\norigin:\n  upstreams: [\"missing-port\"]\n",
        );
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("server.listen")
        );

        let cfg = parse("version: 1\norigin:\n  upstreams: [\"missing-port\"]\n");
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("has no port")
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_vary_headers() {
        for headers in [
            "[\"bad header\"]",
            "[\"Accept-Language\", \"accept-language\"]",
        ] {
            let cfg = parse(&format!(
                "{BASE}routes:\n  - id: r\n    match: /x\n    cache:\n      vary:\n        headers: {headers}\n"
            ));
            assert!(validate(&cfg).is_err());
        }
    }

    #[test]
    fn accepts_requeue_now_that_lock_timeouts_have_an_implemented_path() {
        let cfg = parse(&format!(
            "{BASE}coalesce:\n  wait_timeout: 30s\n  on_timeout: requeue\n"
        ));
        validate(&cfg).unwrap();
    }
}
