//! Unix socket listener: lets other processes (an LED daemon, cron jobs,
//! ad-hoc scripts) push text onto the display, or (`STATUS`) pull a
//! terminal-readable health report back out. Runs on its own thread and
//! hands parsed commands to the main event loop over a channel, so the
//! socket's blocking accept() loop never touches the display/fan state
//! directly.
//!
//! `STATUS` is the one request/response case: everything else here is
//! fire-and-forget (the sender doesn't wait for a reply), but a status
//! report needs live data only the main loop has (fan stall history,
//! active overrides) -- so this asks for one, using a fresh one-shot
//! `mpsc` channel per request as the reply path, and waits (up to a
//! timeout) for the main loop to compute and send one back before writing
//! it to the client and closing the connection.

use std::io::{BufRead, BufReader, Write};
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
    /// A `STATUS` request came in on some connection; send the current
    /// terminal-readable report (see `report.rs`) back on this channel.
    /// One fresh channel per request, not a `Debug`/`PartialEq`-friendly
    /// payload like the other variants, hence the manual `Clone`-only
    /// derive above (`mpsc::Sender` is `Clone` but not `Debug`).
    StatusRequest(Sender<String>),
}

/// Parses one message from a connection: either a `SHOW`/`CLEAR` header
/// followed by up to two content lines, or bare text (shorthand for
/// `SHOW info 5 <line0>\n<line1>`). Header line: `SHOW <level> <ttl>
/// [bay=N]` / `CLEAR [bay=N]`.
fn parse_message(lines: &[String]) -> Option<SocketCommand> {
    let first = lines.first()?.trim();
    let mut parts = first.split_whitespace();

    match parts.next()?.to_ascii_uppercase().as_str() {
        "CLEAR" => Some(SocketCommand::Clear { bay: parse_bay(parts) }),
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

fn handle_connection(stream: UnixStream, tx: &Sender<SocketCommand>) {
    // A second handle to the same socket, kept for writing a STATUS
    // response after the BufReader below has consumed the original for
    // reading -- both directions of a Unix stream socket are independent,
    // so this is fine even though `stream` itself is about to be moved.
    let writer = stream.try_clone().ok();

    let reader = BufReader::new(stream);
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
        // The main loop polls at a ~100ms ceiling, so this should resolve
        // almost immediately; the timeout is just so a client can't hang
        // forever if the daemon's main loop is somehow wedged.
        if let Ok(report) = resp_rx.recv_timeout(Duration::from_secs(5)) {
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
pub fn spawn(path: &str, group: &str, tx: Sender<SocketCommand>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path); // stale socket from a previous run
    let listener = UnixListener::bind(path)?;

    set_socket_perms(path, group);

    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            handle_connection(conn, &tx);
        }
    });
    Ok(())
}

fn set_socket_perms(path: &str, group: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    // Best-effort chgrp; if the group doesn't exist yet, leave root:root and
    // let the operator create it (documented in the systemd unit / README).
    let _ = std::process::Command::new("chgrp").arg(group).arg(path).status();
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
            SocketCommand::Show { level, ttl_secs, bay, line0, line1 } => {
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
            SocketCommand::Show { level, ttl_secs, bay, line0, line1 } => {
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

    #[test]
    fn bare_single_line_leaves_line1_blank() {
        let cmd = parse_message(&lines("just one line")).unwrap();
        match cmd {
            SocketCommand::Show { line1, .. } => assert_eq!(line1, ""),
            _ => panic!("expected Show"),
        }
    }
}
