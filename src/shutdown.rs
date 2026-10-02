//! SIGTERM/SIGINT handling. The handler only sets a flag; the daemon's
//! event loop and `fan-profile`'s sweep check it and exit cleanly, so fans
//! are handed back (see `fan::FanController::restore`) instead of being
//! left at whatever speed they were last commanded.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Installs the handler for SIGTERM and SIGINT. Installed without
/// `SA_RESTART`, so a blocking `poll()` returns `EINTR` promptly instead of
/// waiting out its timeout.
pub fn install() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `sigaction` is zero-initializable plain data, and
        // `on_signal` only performs an atomic store, which is
        // async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(signal, &raw const action, std::ptr::null_mut());
        }
    }
}

/// True once SIGTERM or SIGINT has been received.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigterm_sets_the_flag_instead_of_killing_the_process() {
        install();
        // SAFETY: raising a signal whose handler was just installed above.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        assert!(requested());
    }
}
