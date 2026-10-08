//! The origin server as a child process (`origin.command`).
//!
//! Tanod starts it, waits until its address accepts connections, and only
//! then serves; it stops it after its own drain, so requests already admitted
//! finish first; and if it exits on its own, Tanod exits with its code so the
//! platform restarts both. One container, one systemd unit — no runner
//! process beside the two, and nothing specific to any framework.
//!
//! The child runs in its own process group. A signal sent to Tanod's group —
//! Ctrl+C in a terminal, an init that signals the group — therefore reaches
//! only Tanod, which stops the origin when it is ready to: an origin stopped
//! at the same moment as Tanod would fail the requests Tanod is draining.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::config::schema::OriginCommand;

/// Split `host:port`, unbracketing an IPv6 literal.
pub fn split_upstream(upstream: &str) -> Option<(String, u16)> {
    let (host, port) = upstream.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_string(), port))
}

pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Where `program` would be found: as given if it contains a `/`, otherwise
/// the first executable match on `PATH`, as the exec call will search.
pub fn find_program(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &std::path::Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let path = std::path::PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| std::path::Path::new(dir).join(program))
        .find(|path| executable(path))
}

/// The error [`start`] returns when `interrupted` asked it to stop.
pub const INTERRUPTED: &str = "stopped while waiting for the origin to start";

/// The exit code to report for a child: its own, or 128 + the signal.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(15))
}

// One state, swapped by both sides, so that a child exiting at the same moment
// it became ready, or at the same moment Tanod started stopping it, is seen by
// exactly one of them.
const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const STOPPING: u8 = 2;
const EXITED: u8 = 3;

/// A running origin. Dropping it does not stop the child; call [`stop`].
///
/// [`stop`]: Supervisor::stop
pub struct Supervisor {
    pid: Pid,
    state: Arc<AtomicU8>,
    exited: mpsc::Receiver<ExitStatus>,
    stop_timeout: Duration,
}

/// Start the origin and wait until `upstream` accepts connections.
///
/// `interrupted` is polled while waiting: when it returns true (Tanod was
/// asked to stop before it started serving) the origin is stopped and an
/// error returned, rather than Tanod dying with the origin still running in
/// its own process group.
///
/// `on_unexpected_exit` runs on a background thread if the child exits after
/// it became ready and before [`Supervisor::stop`]; Tanod's binary exits the
/// process from it.
pub fn start(
    cfg: &OriginCommand,
    upstream: &str,
    interrupted: impl FnMut() -> bool,
    on_unexpected_exit: impl FnOnce(ExitStatus) + Send + 'static,
) -> Result<Supervisor, String> {
    let (host, port) =
        split_upstream(upstream).ok_or_else(|| format!("upstream {upstream} is not host:port"))?;
    let program = cfg
        .args
        .first()
        .ok_or_else(|| "origin.command.args is empty".to_string())?;

    let mut command = Command::new(program);
    command
        .args(&cfg.args[1..])
        // The inherited PORT is usually Tanod's own listener; the origin must
        // not try to bind it.
        .env("PORT", port.to_string())
        .env("HOSTNAME", &host)
        .envs(&cfg.env)
        .stdin(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start `{program}`: {e}"))?;
    let pid = Pid::from_raw(i32::try_from(child.id()).map_err(|_| "pid out of range")?);

    let state = Arc::new(AtomicU8::new(STARTING));
    let (tx, exited) = mpsc::sync_channel(1);
    let waiter_state = state.clone();
    let spawned = std::thread::Builder::new()
        .name("origin-wait".to_string())
        .spawn(move || {
            let status = match child.wait() {
                Ok(status) => status,
                Err(e) => {
                    log::error!("waiting for the origin process failed: {e}");
                    return;
                }
            };
            let before = waiter_state.swap(EXITED, Ordering::SeqCst);
            let _ = tx.send(status);
            if before == RUNNING {
                on_unexpected_exit(status);
            }
        });
    if let Err(e) = spawned {
        // Nothing would ever wait for or stop the child: do not leave it.
        let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGKILL);
        return Err(format!("could not start the origin watcher: {e}"));
    }

    let supervisor = Supervisor {
        pid,
        state,
        exited,
        stop_timeout: cfg.stop_timeout.as_duration(),
    };
    supervisor.wait_ready(&host, port, cfg.ready_timeout.as_duration(), interrupted)?;
    Ok(supervisor)
}

impl Supervisor {
    pub fn pid(&self) -> u32 {
        self.pid.as_raw().unsigned_abs()
    }

    fn wait_ready(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
        mut interrupted: impl FnMut() -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state.load(Ordering::SeqCst) == EXITED {
                return Err(self.exited_early());
            }
            if interrupted() {
                self.stop();
                return Err(INTERRUPTED.to_string());
            }
            if accepts(host, port) {
                // Ready, unless it exited in the meantime: the swap decides
                // which side reports that.
                return match self.state.compare_exchange(
                    STARTING,
                    RUNNING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(self.exited_early()),
                };
            }
            if Instant::now() >= deadline {
                self.stop();
                return Err(format!(
                    "the origin did not accept connections on {host}:{port} within {timeout:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn exited_early(&self) -> String {
        match self.exited.recv_timeout(Duration::from_secs(1)) {
            Ok(status) => format!(
                "the origin exited before it accepted connections ({status}, code {})",
                exit_code(status)
            ),
            Err(_) => "the origin exited before it accepted connections".to_string(),
        }
    }

    /// Stop the origin: `SIGTERM` to its process group, then `SIGKILL` after
    /// `stop_timeout`. Returns how it exited, if it did.
    pub fn stop(&self) -> Option<ExitStatus> {
        if self.state.swap(STOPPING, Ordering::SeqCst) == EXITED {
            // The watcher marks the exit before it sends the status, so wait
            // for the send rather than racing it.
            return self.exited.recv_timeout(Duration::from_secs(1)).ok();
        }
        // The group, not just the leader: a server that forks workers must
        // not leave them behind.
        let group = Pid::from_raw(-self.pid.as_raw());
        let _ = kill(group, Signal::SIGTERM);
        match self.exited.recv_timeout(self.stop_timeout) {
            Ok(status) => Some(status),
            Err(_) => {
                log::warn!(
                    "the origin did not exit within {:?} of SIGTERM; sending SIGKILL",
                    self.stop_timeout
                );
                let _ = kill(group, Signal::SIGKILL);
                self.exited.recv_timeout(Duration::from_secs(5)).ok()
            }
        }
    }
}

fn accepts(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .collect::<Vec<SocketAddr>>()
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::units::Dur;
    use std::collections::BTreeMap;
    use std::net::TcpListener;

    fn command(args: &[&str], ready: u64, stop: u64) -> OriginCommand {
        OriginCommand {
            args: args.iter().map(|a| a.to_string()).collect(),
            env: BTreeMap::new(),
            ready_timeout: Dur(Duration::from_secs(ready)),
            stop_timeout: Dur(Duration::from_secs(stop)),
        }
    }

    /// A child that listens on PORT, so readiness is real, using only `sh`
    /// and a listener this test owns: the child's job is to stay alive.
    fn listening_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn upstreams_are_split_and_classified() {
        assert_eq!(
            split_upstream("127.0.0.1:3001"),
            Some(("127.0.0.1".into(), 3001))
        );
        assert_eq!(split_upstream("[::1]:3001"), Some(("::1".into(), 3001)));
        assert_eq!(split_upstream("nohost"), None);
        assert!(is_loopback("127.0.0.1") && is_loopback("::1") && is_loopback("LOCALHOST"));
        assert!(!is_loopback("10.0.0.1") && !is_loopback("web"));
    }

    #[test]
    fn programs_are_found_the_way_exec_finds_them() {
        assert!(find_program("sh").is_some_and(|p| p.ends_with("sh")));
        assert!(find_program("/bin/sh").is_some());
        assert!(find_program("definitely-not-a-program-tanod").is_none());
        assert!(find_program("/etc/hostname").is_none(), "not executable");
    }

    #[test]
    fn a_ready_origin_is_stopped_with_sigterm() {
        let (_listener, port) = listening_port();
        let sup = start(
            &command(&["sh", "-c", "exec sleep 30"], 5, 5),
            &format!("127.0.0.1:{port}"),
            || false,
            |_| panic!("a stop must not count as an unexpected exit"),
        )
        .unwrap();
        let status = sup.stop().expect("the child exited");
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    }

    #[test]
    fn the_origin_gets_port_and_hostname_from_the_upstream() {
        let (_listener, port) = listening_port();
        let dir = std::env::temp_dir().join(format!("tanod-supervise-env-{}", std::process::id()));
        let out = dir.with_extension("txt");
        let script = format!(
            "echo \"$PORT $HOSTNAME $EXTRA\" > {}; exec sleep 30",
            out.display()
        );
        let mut cmd = command(&["sh", "-c", &script], 5, 5);
        cmd.env.insert("EXTRA".into(), "yes".into());
        let sup = start(&cmd, &format!("127.0.0.1:{port}"), || false, |_| {}).unwrap();
        // This test's own listener makes the child "ready" before its first
        // line has run, so wait for the file rather than racing it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n')) {
            assert!(
                Instant::now() < deadline,
                "the child never wrote its environment"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        sup.stop();
        let written = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        assert_eq!(written.trim(), format!("{port} 127.0.0.1 yes"));
    }

    #[test]
    fn an_origin_that_exits_before_it_is_ready_is_an_error() {
        let port = listening_port().1; // listener dropped: nothing will accept
        let err = start(
            &command(&["sh", "-c", "exit 7"], 5, 5),
            &format!("127.0.0.1:{port}"),
            || false,
            |_| panic!("not ready yet, so not unexpected"),
        )
        .err()
        .unwrap();
        assert!(err.contains("exited before"), "{err}");
        assert!(err.contains("code 7"), "{err}");
    }

    #[test]
    fn an_origin_that_never_listens_times_out_and_is_stopped() {
        let port = listening_port().1;
        let started = Instant::now();
        let err = start(
            &command(&["sh", "-c", "exec sleep 30"], 1, 2),
            &format!("127.0.0.1:{port}"),
            || false,
            |_| panic!("stopped by us"),
        )
        .err()
        .unwrap();
        assert!(err.contains("did not accept connections"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_origin_that_ignores_sigterm_is_killed() {
        let (_listener, port) = listening_port();
        let marker =
            std::env::temp_dir().join(format!("tanod-supervise-trap-{}", std::process::id()));
        let script = format!(
            "trap '' TERM; touch {}; while :; do sleep 1; done",
            marker.display()
        );
        let sup = start(
            &command(&["sh", "-c", &script], 5, 1),
            &format!("127.0.0.1:{port}"),
            || false,
            |_| {},
        )
        .unwrap();
        // Ready is instant here (this test owns the listener): wait until the
        // trap is installed, or SIGTERM would land first and prove nothing.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the child never installed its trap"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&marker);
        let status = sup.stop().expect("killed");
        assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
    }

    #[test]
    fn an_unexpected_exit_after_ready_is_reported() {
        let (_listener, port) = listening_port();
        let (tx, rx) = mpsc::channel();
        let _sup = start(
            &command(&["sh", "-c", "sleep 0.5; exit 3"], 5, 5),
            &format!("127.0.0.1:{port}"),
            || false,
            move |status| {
                let _ = tx.send(exit_code(status));
            },
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 3);
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tanod-supervise-{name}-{}", std::process::id()))
    }

    fn wait_for_file(path: &std::path::Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(path)
                && s.ends_with('\n')
            {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn being_stopped_while_waiting_stops_the_origin_instead_of_orphaning_it() {
        let port = listening_port().1; // nothing will ever accept here
        let pidfile = temp_file("interrupted");
        let script = format!("echo $$ > {}; exec sleep 30", pidfile.display());
        let mut polls = 0;
        let started = Instant::now();
        let err = start(
            &command(&["sh", "-c", &script], 30, 5),
            &format!("127.0.0.1:{port}"),
            move || {
                polls += 1;
                polls > 5
            },
            |_| panic!("an interrupted start is not an unexpected exit"),
        )
        .err()
        .unwrap();
        assert!(err.contains("stopped while waiting"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait was not cut short"
        );
        let pid: i32 = wait_for_file(&pidfile).trim().parse().unwrap();
        let _ = std::fs::remove_file(&pidfile);
        // Reaped by the watcher, so the pid no longer exists at all.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            assert!(
                Instant::now() < deadline,
                "the origin outlived an interrupted start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn stop_after_an_unexpected_exit_still_reports_how_it_exited() {
        let (_listener, port) = listening_port();
        let (tx, rx) = mpsc::channel();
        let sup = start(
            &command(&["sh", "-c", "sleep 0.3; exit 5"], 5, 5),
            &format!("127.0.0.1:{port}"),
            || false,
            move |status| {
                let _ = tx.send(exit_code(status));
            },
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 5);
        let status = sup.stop().expect("the status of the earlier exit");
        assert_eq!(status.code(), Some(5));
    }

    #[test]
    fn configured_env_overrides_the_derived_port_and_hostname() {
        let (_listener, port) = listening_port();
        let out = temp_file("override");
        let script = format!(
            "echo \"$PORT $HOSTNAME\" > {}; exec sleep 30",
            out.display()
        );
        let mut cmd = command(&["sh", "-c", &script], 5, 5);
        cmd.env.insert("PORT".into(), "9999".into());
        cmd.env.insert("HOSTNAME".into(), "0.0.0.0".into());
        let sup = start(&cmd, &format!("127.0.0.1:{port}"), || false, |_| {}).unwrap();
        let written = wait_for_file(&out);
        sup.stop();
        let _ = std::fs::remove_file(&out);
        assert_eq!(written.trim(), "9999 0.0.0.0");
    }

    #[test]
    fn the_origin_runs_in_a_process_group_of_its_own() {
        let (_listener, port) = listening_port();
        let out = temp_file("pgid");
        // Fields 1 and 5 of /proc/self/stat are the pid and the process group.
        let script = format!(
            "awk '{{print $1, $5}}' /proc/$$/stat > {}; exec sleep 30",
            out.display()
        );
        let sup = start(
            &command(&["sh", "-c", &script], 5, 5),
            &format!("127.0.0.1:{port}"),
            || false,
            |_| {},
        )
        .unwrap();
        let written = wait_for_file(&out);
        sup.stop();
        let _ = std::fs::remove_file(&out);
        let mut fields = written.split_whitespace();
        let (pid, pgid) = (fields.next().unwrap(), fields.next().unwrap());
        assert_eq!(pid, pgid, "the origin should lead its own group");
        assert_ne!(
            pgid,
            nix::unistd::getpgrp().as_raw().to_string(),
            "the origin shares Tanod's group, so a group signal would stop it mid-drain"
        );
    }
}
