//! Thin wrapper around libc's `syslog(3)`, used for anything worth
//! surfacing to system-level log/alert tooling -- `journalctl -p
//! warning`/`-p crit`, remote syslog forwarding, TrueNAS's own log
//! ingestion -- not just this daemon's own stdout/journal capture.
//!
//! Deliberately real `syslog()` calls, not `eprintln!` with a "WARNING:"
//! text prefix: journald's syslog socket (`/dev/log`) tags each message
//! with its actual priority (the `PRIORITY`/`SYSLOG_PRIORITY` journal
//! fields), which is what makes `journalctl -p warning` and severity-based
//! alerting work at all -- a plain stderr line is all one priority no
//! matter what it says. Messages sent this way still show up in
//! `journalctl -u lcm-status` too (journald attributes a syslog-socket
//! message to the sending process's unit), so nothing is lost from the
//! per-unit view by not also duplicating to stderr.

use std::ffi::CString;
use std::sync::Once;

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        let ident = CString::new("lcm-status").expect("no NUL in literal");
        // LOG_PID: tag each line with our pid, useful across restarts.
        // Leaked deliberately: openlog keeps a pointer to `ident` for the
        // life of the process, and this only runs once.
        unsafe {
            libc::openlog(Box::leak(Box::new(ident)).as_ptr(), libc::LOG_PID, libc::LOG_DAEMON);
        }
    });
}

fn send(priority: libc::c_int, msg: &str) {
    // Sanitized to a plain %s argument rather than interpolated into the
    // format string -- syslog(3)'s format string is real printf(3), so a
    // message containing a stray "%s" would otherwise be a format-string
    // bug, not just a display glitch.
    let Ok(c) = CString::new(msg) else { return }; // msg can't legally contain a NUL anyway
    unsafe {
        libc::syslog(priority, b"%s\0".as_ptr() as *const libc::c_char, c.as_ptr());
    }
}

pub fn info(msg: &str) {
    send(libc::LOG_INFO, msg);
}

/// A condition that was flagged (warning/critical) has cleared. Its own
/// level (not just `info`) so a "back to normal" line is easy to filter
/// for/alert on as its own event, not just absence of further warnings.
pub fn notice(msg: &str) {
    send(libc::LOG_NOTICE, msg);
}

pub fn warning(msg: &str) {
    send(libc::LOG_WARNING, msg);
}

pub fn critical(msg: &str) {
    send(libc::LOG_CRIT, msg);
}
