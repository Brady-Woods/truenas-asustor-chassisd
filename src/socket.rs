//! Unix socket listener: lets other processes (an LED daemon, cron jobs,
//! ad-hoc scripts) push text onto the display, blink a drive bay's LEDs
//! to find it (`LOCATE`), or (`STATUS`) pull a terminal-readable health
//! report back out. Runs on its own thread and
//! hands parsed commands to the main event loop over a channel, so the
//! socket's blocking `accept()` loop never touches the display/fan state
//! directly.
//!
//! `STATUS` is the one request/response case: everything else here is
//! fire-and-forget (the sender doesn't wait for a reply), but a status
//! report needs live data only the main loop has (fan stall history,
//! active overrides) -- so this asks for one, using a fresh one-shot
//! `mpsc` channel per request as the reply path, and waits (up to a
//! timeout) for the main loop to compute and send one back before writing
//! it to the client and closing the connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Ordered so `a < b` means "b is at least as severe" -- used both for the
/// LCD override precedence (a higher level can replace a lower one, never
/// the reverse) and for the status LED color/pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Error,
    Critical,
}

impl Level {
    fn parse(s: &str) -> Level {
        match s.to_ascii_lowercase().as_str() {
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            "critical" => Level::Critical,
            _ => Level::Info,
        }
    }

    /// Error and critical are meant to always be seen: they persist on the
    /// LCD (rather than expiring on a TTL) and wake the panel from sleep.
    pub fn always_visible(self) -> bool {
        self >= Level::Error
    }
}

#[derive(Clone)]
pub enum SocketCommand {
    Show {
        level: Level,
        ttl_secs: u64,
        /// Optional bay this alert is about (e.g. from `SHOW error 0
        /// bay=2`). error/critical with a bay set also flashes that bay's
        /// red LED (a distinct "alert" pattern from a confirmed SMART
        /// failure), not just the general status LED.
        bay: Option<u32>,
        line0: String,
        line1: String,
    },
    Clear {
        bay: Option<u32>,
    },
    /// `LOCATE [bay=N] [ttl_secs]`: blink a bay's LEDs (or, with no bay,
    /// the whole chassis's: power + status LEDs, hostname on the LCD) so
    /// someone standing at the rack can find it -- ADM's "inspection LED".
    /// Lasts `ttl_secs` (default `DEFAULT_LOCATE_TTL_SECS`; `0` = until
    /// `LOCATE off`). Independent of `SHOW`/`CLEAR`: a bare `CLEAR` from
    /// some script tidying up its own alert doesn't cut a person's locate
    /// short. Repeating it for something already blinking restarts its TTL.
    Locate {
        bay: Option<u32>,
        ttl_secs: u64,
    },
    /// `LOCATE off [bay=N]`: stop locating that bay, or with no bay, stop
    /// every active locate (chassis and all bays) -- the "make it stop"
    /// button doesn't need to know what was started.
    LocateOff {
        bay: Option<u32>,
    },
    /// A `STATUS` request came in on some connection; send the current
    /// terminal-readable report (see `report.rs`) back on this channel.
    /// One fresh channel per request, not a `Debug`/`PartialEq`-friendly
    /// payload like the other variants, hence the manual `Clone`-only
    /// derive above (`mpsc::Sender` is `Clone` but not `Debug`).
    StatusRequest(Sender<String>),
}

/// How long a `LOCATE` that doesn't give a TTL keeps blinking: long enough
/// to walk to the rack, short enough that a forgotten one stops by itself.
pub const DEFAULT_LOCATE_TTL_SECS: u64 = 60;

/// Parses one message from a connection: either a `SHOW`/`CLEAR`/`LOCATE`
/// header followed by up to two content lines, or bare text (shorthand for
/// `SHOW info 5 <line0>\n<line1>`). Header line: `SHOW <level> <ttl>
/// [bay=N]` / `CLEAR [bay=N]` / `LOCATE [off] [bay=N] [ttl]`.
fn parse_message(lines: &[String]) -> Option<SocketCommand> {
    let first = lines.first()?.trim();
    let mut parts = first.split_whitespace();

    match parts.next()?.to_ascii_uppercase().as_str() {
        "CLEAR" => Some(SocketCommand::Clear {
            bay: parse_bay(parts),
        }),
        "LOCATE" => parse_locate(parts),
        "SHOW" => {
            let level = Level::parse(parts.next()?);
            let ttl_secs = parts.next()?.parse().ok()?;
            let bay = parse_bay(parts);
            Some(SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0: lines.get(1).cloned().unwrap_or_default(),
                line1: lines.get(2).cloned().unwrap_or_default(),
            })
        }
        _ => {
            // Bare text shorthand: first line -> line0, second -> line1.
            Some(SocketCommand::Show {
                level: Level::Info,
                ttl_secs: 5,
                bay: None,
                line0: lines.first().cloned().unwrap_or_default(),
                line1: lines.get(1).cloned().unwrap_or_default(),
            })
        }
    }
}

fn parse_bay<'a>(mut parts: impl Iterator<Item = &'a str>) -> Option<u32> {
    parts.find_map(|p| p.strip_prefix("bay=").and_then(|n| n.parse().ok()))
}

/// The arguments after `LOCATE`, in any order: `off`, `bay=N`, and a TTL
/// in seconds. Stricter than `SHOW`'s `parse_bay` on purpose: a typo'd
/// `bay=` (or `bay=0` -- bays count from 1) rejects the whole request
/// rather than silently falling back to locating the entire chassis.
fn parse_locate<'a>(parts: impl Iterator<Item = &'a str>) -> Option<SocketCommand> {
    let (mut off, mut bay, mut ttl_secs) = (false, None, None);
    for part in parts {
        if part.eq_ignore_ascii_case("off") {
            off = true;
        } else if let Some(n) = part.strip_prefix("bay=") {
            bay = Some(n.parse().ok().filter(|&b| b > 0)?);
        } else {
            ttl_secs = Some(part.parse().ok()?);
        }
    }
    if off {
        // A TTL on a request to stop makes no sense; refuse rather than guess.
        ttl_secs
            .is_none()
            .then_some(SocketCommand::LocateOff { bay })
    } else {
        Some(SocketCommand::Locate {
            bay,
            ttl_secs: ttl_secs.unwrap_or(DEFAULT_LOCATE_TTL_SECS),
        })
    }
}

/// The `LOCATE` header line `parse_locate` accepts, for the client side
/// (`lcm-status locate`). `ttl_secs: None` leaves the TTL to the daemon.
pub fn locate_request(bay: Option<u32>, ttl_secs: Option<u64>, off: bool) -> String {
    let mut parts = vec!["LOCATE".to_string()];
    if off {
        parts.push("off".to_string());
    }
    parts.extend(bay.map(|b| format!("bay={b}")));
    parts.extend(ttl_secs.map(|t| t.to_string()));
    parts.join(" ")
}

/// How long a client may take to send its request (and, for `STATUS`,
/// to accept the reply) before the connection is dropped.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a `STATUS` request waits for the main loop to build the
/// report. Generous because building it runs `zpool`/`smartctl`, each of
/// which `hal` allows up to 10s before giving up.
const STATUS_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Requests are a header plus two short lines; anything past this is
/// ignored.
const MAX_REQUEST_BYTES: u64 = 4096;

fn handle_connection(stream: UnixStream, tx: &Sender<SocketCommand>) {
    // A client that connects and then never sends (or never closes) only
    // ties up its own thread, and only until this expires.
    let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));

    // A second handle to the same socket, kept for writing a STATUS
    // response after the BufReader below has consumed the original for
    // reading -- both directions of a Unix stream socket are independent,
    // so this is fine even though `stream` itself is about to be moved.
    let writer = stream.try_clone().ok();

    let reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    if lines.is_empty() {
        return;
    }

    if lines[0].trim().eq_ignore_ascii_case("STATUS") {
        let Some(mut writer) = writer else { return };
        let (resp_tx, resp_rx) = std::sync::mpsc::channel();
        if tx.send(SocketCommand::StatusRequest(resp_tx)).is_err() {
            return;
        }
        // Normally resolves within one ~100ms main-loop pass; the timeout
        // is so a client can't hang forever if the main loop is wedged.
        if let Ok(report) = resp_rx.recv_timeout(STATUS_REPLY_TIMEOUT) {
            let _ = writer.write_all(report.as_bytes());
        }
        return;
    }

    if let Some(cmd) = parse_message(&lines) {
        let _ = tx.send(cmd);
    }
}

/// Starts the socket listener on its own thread. Returns immediately;
/// parsed commands arrive on `tx`.
///
/// Fails with `ErrorKind::AddrInUse` if another daemon is already
/// accepting connections on `path`, rather than unlinking its socket out
/// from under it.
pub fn spawn(path: &str, group: &str, tx: Sender<SocketCommand>) -> std::io::Result<()> {
    if UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another lcm-status daemon is already listening there",
        ));
    }
    let _ = std::fs::remove_file(path); // stale socket from a previous run
    let listener = UnixListener::bind(path)?;

    set_socket_perms(path, group);

    // One thread per connection, so a slow client (or a STATUS request
    // waiting on the main loop) never holds up anyone else's.
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || handle_connection(conn, &tx));
        }
    });
    Ok(())
}

fn set_socket_perms(path: &str, group: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    // Best-effort chgrp; if the group doesn't exist yet, leave root:root and
    // let the operator create it (documented in the systemd unit / README).
    let _ = std::process::Command::new("chgrp")
        .arg(group)
        .arg(path)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn parses_show_critical() {
        let cmd = parse_message(&lines("SHOW critical 0\nDISK FAILURE\nCheck bay 3")).unwrap();
        match cmd {
            SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0,
                line1,
            } => {
                assert_eq!(level, Level::Critical);
                assert_eq!(ttl_secs, 0);
                assert_eq!(bay, None);
                assert_eq!(line0, "DISK FAILURE");
                assert_eq!(line1, "Check bay 3");
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_show_with_bay() {
        let cmd = parse_message(&lines("SHOW error 0 bay=2\nDISK DEAD\nBay 2 offline")).unwrap();
        match cmd {
            SocketCommand::Show { level, bay, .. } => {
                assert_eq!(level, Level::Error);
                assert_eq!(bay, Some(2));
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_clear_with_bay() {
        let cmd = parse_message(&lines("CLEAR bay=2")).unwrap();
        match cmd {
            SocketCommand::Clear { bay } => assert_eq!(bay, Some(2)),
            _ => panic!("expected Clear"),
        }
    }

    fn locate(header: &str) -> Option<SocketCommand> {
        parse_message(&lines(header))
    }

    #[test]
    fn parses_locate_forms() {
        assert!(matches!(
            locate("LOCATE"),
            Some(SocketCommand::Locate {
                bay: None,
                ttl_secs: DEFAULT_LOCATE_TTL_SECS
            })
        ));
        assert!(matches!(
            locate("locate bay=3"),
            Some(SocketCommand::Locate {
                bay: Some(3),
                ttl_secs: DEFAULT_LOCATE_TTL_SECS
            })
        ));
        // Arguments in any order; 0 = until LOCATE off.
        assert!(matches!(
            locate("LOCATE 0 bay=2"),
            Some(SocketCommand::Locate {
                bay: Some(2),
                ttl_secs: 0
            })
        ));
        assert!(matches!(
            locate("LOCATE OFF"),
            Some(SocketCommand::LocateOff { bay: None })
        ));
        assert!(matches!(
            locate("LOCATE bay=4 off"),
            Some(SocketCommand::LocateOff { bay: Some(4) })
        ));
    }

    #[test]
    fn malformed_locate_is_rejected_not_widened_to_the_chassis() {
        assert!(locate("LOCATE bay=two").is_none());
        assert!(locate("LOCATE bay=0").is_none());
        assert!(locate("LOCATE bay=2 soon").is_none());
        assert!(locate("LOCATE off 30").is_none());
    }

    #[test]
    fn locate_request_round_trips() {
        for (bay, ttl, off) in [
            (None, None, false),
            (Some(2), None, false),
            (Some(1), Some(300), false),
            (None, Some(0), false),
            (None, None, true),
            (Some(3), None, true),
        ] {
            let parsed = locate(&locate_request(bay, ttl, off)).unwrap();
            match parsed {
                SocketCommand::Locate { bay: b, ttl_secs } => {
                    assert!(!off);
                    assert_eq!(b, bay);
                    assert_eq!(ttl_secs, ttl.unwrap_or(DEFAULT_LOCATE_TTL_SECS));
                }
                SocketCommand::LocateOff { bay: b } => {
                    assert!(off);
                    assert_eq!(b, bay);
                }
                _ => panic!("expected Locate/LocateOff"),
            }
        }
    }

    #[test]
    fn error_and_critical_are_always_visible() {
        assert!(Level::Error.always_visible());
        assert!(Level::Critical.always_visible());
        assert!(!Level::Warn.always_visible());
        assert!(!Level::Info.always_visible());
    }

    #[test]
    fn parses_bare_text_shorthand() {
        let cmd = parse_message(&lines("hello there\nsecond line")).unwrap();
        match cmd {
            SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0,
                line1,
            } => {
                assert_eq!(level, Level::Info);
                assert_eq!(ttl_secs, 5);
                assert_eq!(bay, None);
                assert_eq!(line0, "hello there");
                assert_eq!(line1, "second line");
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_clear() {
        assert!(matches!(
            parse_message(&lines("CLEAR")),
            Some(SocketCommand::Clear { bay: None })
        ));
    }

    fn temp_socket_path(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("lcm-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn refuses_to_take_over_a_live_socket() {
        let path = temp_socket_path("live");
        let _running = UnixListener::bind(&path).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let err = spawn(&path, "nogroup", tx).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(std::path::Path::new(&path).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replaces_a_stale_socket() {
        let path = temp_socket_path("stale");
        // A leftover entry nothing is listening on. (A plain file rather
        // than a bound-then-dropped listener: on macOS a child spawned by
        // another test can inherit and hold the listener open.)
        std::fs::write(&path, "").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        spawn(&path, "nogroup", tx).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stuck_client_does_not_block_others_and_is_eventually_dropped() {
        let path = temp_socket_path("stuck");
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(&path, "nogroup", tx).unwrap();

        // Connects, sends nothing, never closes.
        let mut stuck = UnixStream::connect(&path).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"hello\nworld\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let cmd = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("second client blocked behind the stuck one");
        assert!(matches!(cmd, SocketCommand::Show { line0, .. } if line0 == "hello"));

        // The daemon side gives up on the stuck client: we see EOF.
        stuck.set_read_timeout(Some(CLIENT_IO_TIMEOUT * 3)).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(stuck.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bare_single_line_leaves_line1_blank() {
        let cmd = parse_message(&lines("just one line")).unwrap();
        match cmd {
            SocketCommand::Show { line1, .. } => assert_eq!(line1, ""),
            _ => panic!("expected Show"),
        }
    }
}
