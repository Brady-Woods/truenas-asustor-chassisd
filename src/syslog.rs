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

/// Opens the syslog connection, tagged `lcm-status[pid]` under the daemon
/// facility. Optional -- `syslog(3)` opens one implicitly -- but sets the
/// ident and `LOG_PID`.
pub fn init() {
    // SAFETY: the ident is a `'static` C string literal; openlog keeps
    // that pointer for the life of the process, which it outlives.
    unsafe {
        libc::openlog(c"lcm-status".as_ptr(), libc::LOG_PID, libc::LOG_DAEMON);
    }
}

fn send(priority: libc::c_int, msg: &str) {
    // Keep unit tests from writing to the host's system log.
    if cfg!(test) {
        return;
    }
    // A NUL would truncate (or, unescaped, drop) the message; one can only
    // arrive via text read from sysfs or a tool's output.
    let Ok(msg) = CString::new(msg.replace('\0', "\\0")) else {
        return;
    };
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    // The message is passed as a `%s` argument, never as the format
    // string itself -- syslog(3)'s format is real printf(3), so a stray
    // `%s` in a message would otherwise be a format-string bug.
    unsafe {
        libc::syslog(priority, c"%s".as_ptr(), msg.as_ptr());
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
