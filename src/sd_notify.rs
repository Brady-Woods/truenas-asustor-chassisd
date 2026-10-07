//! Just enough of systemd's `sd_notify(3)` to feed the service watchdog
//! (`WatchdogSec=` in `lcm-status.service`), in safe std: one datagram,
//! `WATCHDOG=1`, to the socket named by `$NOTIFY_SOCKET` (a filesystem
//! path, or an abstract socket when it starts with `@`).
//!
//! The daemon pings only while fan control is demonstrably alive (see
//! `event_loop`), so a wedged fan thread -- or a wedged main loop -- gets
//! the process killed and restarted by systemd instead of silently leaving
//! a fan unsupervised. Without `NOTIFY_SOCKET`/`WATCHDOG_USEC` (run by
//! hand, or a unit without `WatchdogSec=`) every call is a no-op.

use std::io;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::{Duration, Instant};

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
