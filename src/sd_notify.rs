//! Just enough of systemd's `sd_notify(3)` to feed the service watchdog
//! (`WatchdogSec=` in `lcm-status.service`), in safe std: one datagram,
//! `WATCHDOG=1`, to the socket named by `$NOTIFY_SOCKET` (a filesystem
//! path, or an abstract socket when it starts with `@`).
//!
//! The pings come from a dedicated thread (`spawn_feeder`), not from the
//! main loop: the main loop legitimately blocks for a long time inside
//! subprocess-heavy refreshes (`zpool`, `smartctl`, ... each capped at
//! 10s, several in a row), and a ping scheduled between them would starve
//! on a merely slow box and get a healthy daemon killed. The feeder pings
//! only while the fan thread's heartbeat is fresh *and* the main loop has
//! made progress (`Progress`) within a generous bound, so a wedged fan
//! thread, or a main loop that is truly stuck rather than slow, still stops
//! the pings and systemd restarts the daemon. Without
//! `NOTIFY_SOCKET`/`WATCHDOG_USEC` (run by hand, or a unit without
//! `WatchdogSec=`) every call is a no-op and no thread is started.

use std::io;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How long the main loop may go without reporting progress before the
/// feeder stops pinging. Far above the slowest legitimate pass (a run of
/// 10s-capped subprocesses), far below "forever".
pub const MAIN_LOOP_STALL_AFTER: Duration = Duration::from_secs(300);

/// The main loop's progress marker: bumped every loop iteration and around
/// each blocking refresh, read by the feeder thread.
#[derive(Debug, Clone)]
pub struct Progress {
    last_ms: Arc<AtomicU64>,
    epoch: Instant,
}

impl Progress {
    pub fn new() -> Self {
        Progress {
            last_ms: Arc::new(AtomicU64::new(0)),
            epoch: Instant::now(),
        }
    }

    /// Records that the main loop is alive right now.
    pub fn bump(&self) {
        let ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// Time since the last `bump` (since creation if never bumped).
    pub fn age(&self) -> Duration {
        self.epoch
            .elapsed()
            .saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// The feeder's decision: ping only if the fan thread's heartbeat and the
/// main loop's progress are both young enough.
pub fn should_ping(fan_age: Duration, fan_limit: Duration, progress: &Progress) -> bool {
    fan_age <= fan_limit && progress.age() <= MAIN_LOOP_STALL_AFTER
}

/// Starts the watchdog feeder thread: every `tick` it calls `healthy` and,
/// if that says yes, pings (`Watchdog::ping` rate-limits to half the
/// period). Returns `Ok(false)` without starting anything when there is no
/// systemd watchdog to feed. The thread ends once `stop` is set.
pub fn spawn_feeder(
    watchdog: Watchdog,
    tick: Duration,
    stop: Arc<AtomicBool>,
    healthy: impl Fn() -> bool + Send + 'static,
) -> io::Result<bool> {
    if watchdog.target.is_none() {
        return Ok(false);
    }
    let mut watchdog = watchdog;
    thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if healthy() {
                    watchdog.ping();
                }
                thread::sleep(tick);
            }
        })?;
    Ok(true)
}

/// Sends `WATCHDOG=1` to systemd, at most once per half the watchdog
/// period (systemd's own recommendation).
pub struct Watchdog {
    target: Option<Target>,
    interval: Duration,
    last_sent: Option<Instant>,
    warned: bool,
}

struct Target {
    socket: UnixDatagram,
    addr: SocketAddr,
}

impl Watchdog {
    /// From the environment systemd gives the service.
    pub fn from_env() -> Self {
        let var = |name| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        Self::new(
            var("NOTIFY_SOCKET").as_deref(),
            var("WATCHDOG_USEC").as_deref(),
        )
    }

    /// `notify_socket`/`watchdog_usec` are the values of `$NOTIFY_SOCKET`
    /// and `$WATCHDOG_USEC`. Anything missing or malformed disables the
    /// watchdog (logged if it looks like a mistake rather than "not under
    /// systemd").
    fn new(notify_socket: Option<&str>, watchdog_usec: Option<&str>) -> Self {
        let disabled = Watchdog {
            target: None,
            interval: Duration::ZERO,
            last_sent: None,
            warned: false,
        };
        let (Some(path), Some(usec)) = (notify_socket.filter(|s| !s.is_empty()), watchdog_usec)
        else {
            return disabled;
        };
        let Some(usec) = usec.trim().parse::<u64>().ok().filter(|&u| u > 0) else {
            crate::syslog::warning(&format!("ignoring invalid WATCHDOG_USEC '{usec}'"));
            return disabled;
        };
        match Target::open(path) {
            Ok(target) => Watchdog {
                target: Some(target),
                interval: Duration::from_micros(usec / 2),
                ..disabled
            },
            Err(e) => {
                crate::syslog::warning(&format!(
                    "systemd watchdog disabled: cannot use NOTIFY_SOCKET '{path}': {e}"
                ));
                disabled
            }
        }
    }

    /// True if there is a systemd watchdog to feed.
    #[cfg(test)]
    fn enabled(&self) -> bool {
        self.target.is_some()
    }

    /// Sends a keep-alive if one is due. Never blocks (the socket is
    /// non-blocking); a failure is logged once and retried next call.
    pub fn ping(&mut self) {
        let Some(target) = &self.target else {
            return;
        };
        if self.last_sent.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        match target.socket.send_to_addr(b"WATCHDOG=1", &target.addr) {
            Ok(_) => {
                self.last_sent = Some(Instant::now());
                self.warned = false;
            }
            Err(e) => {
                if !self.warned {
                    self.warned = true;
                    crate::syslog::warning(&format!("could not ping the systemd watchdog: {e}"));
                }
            }
        }
    }
}

impl Target {
    fn open(path: &str) -> io::Result<Self> {
        let addr = parse_addr(path)?;
        let socket = UnixDatagram::unbound()?;
        socket.set_nonblocking(true)?;
        Ok(Target { socket, addr })
    }
}

/// `@name` is an abstract-namespace socket (Linux), anything else a path.
fn parse_addr(path: &str) -> io::Result<SocketAddr> {
    match path.strip_prefix('@') {
        #[cfg(target_os = "linux")]
        Some(name) => {
            use std::os::linux::net::SocketAddrExt;
            SocketAddr::from_abstract_name(name.as_bytes())
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "abstract sockets need Linux",
        )),
        None => SocketAddr::from_pathname(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pings_need_a_fresh_fan_heartbeat_and_main_loop_progress() {
        let p = Progress::new();
        let limit = Duration::from_secs(15);
        assert!(should_ping(Duration::ZERO, limit, &p));
        assert!(should_ping(limit, limit, &p));
        // A stalled fan thread stops the pings even with a busy main loop.
        assert!(!should_ping(limit + Duration::from_secs(1), limit, &p));
        // A main loop silent past the bound stops them too.
        let stale = Progress {
            last_ms: Arc::new(AtomicU64::new(0)),
            epoch: Instant::now()
                .checked_sub(MAIN_LOOP_STALL_AFTER + Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
        };
        if stale.age() > MAIN_LOOP_STALL_AFTER {
            assert!(!should_ping(Duration::ZERO, limit, &stale));
            stale.bump();
            assert!(should_ping(Duration::ZERO, limit, &stale));
        }
    }

    #[test]
    fn a_slow_but_progressing_main_loop_is_not_stalled() {
        // Many seconds inside one blocking refresh is normal: nowhere near
        // the bound.
        let p = Progress {
            last_ms: Arc::new(AtomicU64::new(0)),
            epoch: Instant::now()
                .checked_sub(Duration::from_secs(40))
                .unwrap_or_else(Instant::now),
        };
        assert!(should_ping(Duration::ZERO, Duration::from_secs(15), &p));
    }

    #[test]
    fn the_feeder_pings_only_while_healthy_and_never_without_a_watchdog() {
        let path = scratch("feeder");
        drop(std::fs::remove_file(&path));
        let listener = UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let healthy = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        // 200ms watchdog => a ping at most every 100ms.
        let w = Watchdog::new(path.to_str(), Some("200000"));
        let started = spawn_feeder(w, Duration::from_millis(10), Arc::clone(&stop), {
            let healthy = Arc::clone(&healthy);
            move || healthy.load(Ordering::Relaxed)
        })
        .unwrap();
        assert!(started);
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
        // Unhealthy: drain what is in flight, then expect silence for a
        // window 10x the ping interval.
        healthy.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(300));
        listener.set_nonblocking(true).unwrap();
        while recv(&listener).is_some() {}
        thread::sleep(Duration::from_secs(1));
        assert_eq!(recv(&listener), None, "pinged while unhealthy");
        // Healthy again: pings resume.
        healthy.store(true, Ordering::Relaxed);
        listener.set_nonblocking(false).unwrap();
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
        stop.store(true, Ordering::Relaxed);
        drop(std::fs::remove_file(&path));

        // No watchdog configured: no thread, no error.
        let none = spawn_feeder(
            Watchdog::new(None, None),
            Duration::from_millis(10),
            Arc::new(AtomicBool::new(false)),
            || true,
        )
        .unwrap();
        assert!(!none);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lcm-notify-{}-{tag}", std::process::id()))
    }

    fn recv(sock: &UnixDatagram) -> Option<String> {
        let mut buf = [0u8; 64];
        let n = sock.recv(&mut buf).ok()?;
        Some(String::from_utf8_lossy(buf.get(..n)?).into_owned())
    }

    #[test]
    fn no_notify_socket_or_watchdog_is_a_no_op() {
        for (sock, usec) in [
            (None, None),
            (Some("/run/systemd/notify"), None),
            (None, Some("30000000")),
            (Some(""), Some("30000000")),
            (Some("/nonexistent/x"), Some("0")),
            (Some("/nonexistent/x"), Some("soon")),
        ] {
            let mut w = Watchdog::new(sock, usec);
            assert!(!w.enabled(), "{sock:?} {usec:?}");
            w.ping(); // must not panic or block
        }
    }

    #[test]
    fn pings_a_path_socket_at_most_every_half_period() {
        let path = scratch("path");
        drop(std::fs::remove_file(&path));
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        // 400ms watchdog: a ping at most every 200ms.
        let mut w = Watchdog::new(path.to_str(), Some("400000"));
        assert!(w.enabled());
        w.ping();
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
        w.ping();
        assert_eq!(recv(&listener), None, "rate-limited");
        std::thread::sleep(Duration::from_millis(220));
        w.ping();
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
        drop(std::fs::remove_file(&path));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pings_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("lcm-status-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut w = Watchdog::new(Some(&format!("@{name}")), Some("2000000"));
        assert!(w.enabled());
        w.ping();
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
    }

    #[test]
    fn a_dead_socket_is_not_fatal_and_is_retried() {
        let path = scratch("dead");
        drop(std::fs::remove_file(&path));
        let mut w = Watchdog::new(path.to_str(), Some("400000"));
        // Sending to a path nobody listens on fails; the daemon carries on.
        w.ping();
        assert!(w.last_sent.is_none());
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        w.ping();
        assert_eq!(recv(&listener).as_deref(), Some("WATCHDOG=1"));
        drop(std::fs::remove_file(&path));
    }
}
