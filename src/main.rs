mod config;
mod fan;
mod fan_calibrate;
mod hal;
mod led;
mod monitor;
mod protocol;
mod report;
mod shutdown;
mod socket;
mod state;
mod syslog;
mod template;

use config::Config;
use protocol::{LCM_DEVICE, Lcm, OP_COMMAND, SUB_VERSION};
use state::{Action, AppState, Effect};
use std::os::unix::io::AsRawFd;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Upper bound on how long the event loop sleeps waiting for a panel
/// frame. A fixed 100ms keeps scroll steps and timers responsive without
/// per-state deadline math; one wakeup per 100ms at idle is negligible.
const POLL_INTERVAL_MS: i32 = 100;

const USAGE: &str = "\
usage: lcm-status [daemon] [CONFIG]       run the daemon (default config: /etc/lcm-status.toml)
       lcm-status status [CONFIG]         print the running daemon's health report
       lcm-status check-config [CONFIG]   parse and print a config file
       lcm-status hal-test [CONFIG]       print every screen rendered with that config
       lcm-status fan-profile [-y] [CONFIG]
                                          discover and calibrate fans (stops the daemon)
       lcm-status init                    send the LCD power-on sequence
       lcm-status settext LINE TEXT       write up to 16 chars to line 0 or 1
       lcm-status listen [SECS]           print unsolicited panel frames";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Daemon(PathBuf),
    Status(PathBuf),
    CheckConfig(PathBuf),
    HalTest(PathBuf),
    FanProfile {
        config: PathBuf,
        assume_yes: bool,
    },
    /// `init` / `settext` / `listen`: low-level panel probes, which parse
    /// their own remaining arguments.
    Probe(String),
    Help,
}

/// Parses `args` (including `argv[0]`). Anything unrecognized is an error
/// rather than a config path: treating a mistyped subcommand as a path
/// used to start a second daemon, which removed the running one's socket.
/// The one bare-path form still accepted is the daemon's legacy
/// `lcm-status /path/to/config.toml`, recognized by looking like a path.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let config_arg = |args: &[&str]| -> Result<PathBuf, String> {
        match args {
            [] => Ok(PathBuf::from(config::DEFAULT_CONFIG_PATH)),
            [path] if !path.starts_with('-') => Ok(PathBuf::from(path)),
            _ => Err(format!("unexpected arguments: {}", args.join(" "))),
        }
    };
    match rest.as_slice() {
        [] => config_arg(&[]).map(Cli::Daemon),
        ["daemon", tail @ ..] => config_arg(tail).map(Cli::Daemon),
        ["status", tail @ ..] => config_arg(tail).map(Cli::Status),
        ["check-config", tail @ ..] => config_arg(tail).map(Cli::CheckConfig),
        ["hal-test", tail @ ..] => config_arg(tail).map(Cli::HalTest),
        ["fan-profile", tail @ ..] => {
            let assume_yes = tail.iter().any(|a| matches!(*a, "-y" | "--yes"));
            let positional: Vec<&str> = tail
                .iter()
                .copied()
                .filter(|a| !matches!(*a, "-y" | "--yes"))
                .collect();
            Ok(Cli::FanProfile {
                config: config_arg(&positional)?,
                assume_yes,
            })
        }
        [cmd @ ("init" | "settext" | "listen"), ..] => Ok(Cli::Probe((*cmd).to_string())),
        ["help" | "-h" | "--help", ..] => Ok(Cli::Help),
        [path]
            if path.contains('/')
                || Path::new(path)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("toml")) =>
        {
            Ok(Cli::Daemon(PathBuf::from(path)))
        }
        [cmd, ..] => Err(format!("unknown command '{cmd}'")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("lcm-status: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match cli {
        Cli::Help => println!("{USAGE}"),
        Cli::CheckConfig(path) => {
            let (cfg, diagnostics) = Config::load_with_diagnostics(&path);
            println!("{cfg:#?}");
            for d in &diagnostics {
                eprintln!("{}: {d}", path.display());
            }
            if !diagnostics.is_empty() {
                return ExitCode::FAILURE;
            }
        }
        Cli::HalTest(path) => {
            // Uses the real config (not defaults) so [templates.*] edits can be
            // previewed without restarting the daemon.
            let cfg = Config::load(&path);
            let t = &cfg.templates;
            println!("-- network --\n{:#?}", hal::network(&t.network));
            let pools = hal::pools();
            println!(
                "-- pools --\n{:#?}",
                hal::pool_screens(&t.pool, pools.as_deref())
            );
            println!("-- hdd --\n{:#?}", hal::hdd(&cfg, &hal::disks()));
            println!("-- temperature/fan --\n{:#?}", hal::cpu_and_fan(&cfg));
            println!(
                "-- docker issues --\n{:#?}",
                hal::docker_issues(&cfg.docker.ignore, &t.docker)
            );
        }
        Cli::Probe(cmd) => run_probe_command(&cmd, &args),
        Cli::FanProfile { config, assume_yes } => fan_calibrate::run(&config, assume_yes),
        Cli::Status(path) => request_status(&Config::load(&path).socket.path),
        Cli::Daemon(path) => run_daemon(&path),
    }
    ExitCode::SUCCESS
}

/// Client side of the `STATUS` request: connect to the running daemon's
/// own socket, ask for a report, print it, exit. Talks to whatever's
/// actually running -- not a fresh one-shot snapshot the way `hal-test`
/// is -- so it reflects live accumulated state (a fan's stall history, an
/// active override) a brand new process invocation couldn't know about.
fn request_status(socket_path: &str) {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to connect to {socket_path}: {e} (is lcm-status running?)");
            std::process::exit(1);
        }
    };
    if let Err(e) = stream.write_all(b"STATUS\n") {
        eprintln!("failed to send request: {e}");
        std::process::exit(1);
    }
    // Signals "done sending" without closing the read half -- the daemon's
    // BufReader.lines() needs to see EOF on its read side to stop waiting
    // for more input, but we still need to read its response afterward.
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut response = String::new();
    if let Err(e) = stream.read_to_string(&mut response) {
        eprintln!("failed to read response: {e}");
        std::process::exit(1);
    }
    print!("{response}");
}

fn run_daemon(cfg_path: &Path) {
    syslog::init();
    shutdown::install();

    let cfg = Config::load(cfg_path);
    syslog::info(&format!(
        "starting: {} fan(s) configured, temperature warn/critical at {:.0}C/{:.0}C",
        cfg.fans.iter().filter(|f| f.enabled).count(),
        cfg.temperature.warn_threshold,
        cfg.temperature.critical_threshold,
    ));

    // First, before touching the serial port or fans: if another daemon
    // is already running, this is a second instance and must not start.
    let (tx, rx) = mpsc::channel();
    let socket_path = &cfg.socket.path;
    match socket::spawn(socket_path, &cfg.socket.group, tx) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("{socket_path}: {e}; exiting");
            std::process::exit(1);
        }
        Err(e) => eprintln!("socket listener failed to start on {socket_path}: {e}"),
    }

    let mut lcm = Lcm::open(&cfg.display.serial_device).unwrap_or_else(|e| {
        eprintln!("failed to open {}: {e}", cfg.display.serial_device);
        std::process::exit(1);
    });

    let _ = lcm.init();

    // deploy.sh already gates on this before it will even build; this is
    // defense-in-depth for the binary being started some other way. LED
    // writes against a missing driver are harmless no-ops (plain sysfs
    // writes to paths that don't exist), so this doesn't refuse to start
    // -- it just makes sure that degraded state is loud in the logs
    // instead of silently invisible.
    if !led::driver_present() {
        eprintln!(
            "WARNING: asustor-platform-driver (nas-deploy branch: main + PRs #46/#47/#48, \
             https://github.com/mafredri/asustor-platform-driver/pulls) not detected -- \
             LED control (bay/status LEDs, LCD sleep) will silently no-op. \
             LCD text/menu still works. Run deploy.sh, which checks this before building."
        );
    }

    // Blink triggers (RAID-degraded flash, critical-alert flash, bay
    // standby flash) need this loaded; it's not on by default on TrueNAS.
    if !led::ledtrig_timer_loaded() {
        let _ = std::process::Command::new("modprobe")
            .arg("ledtrig-timer")
            .status();
    }

    // Started before the first (slow, subprocess-heavy) refresh below so
    // fan control is never waiting on it.
    let fans = fan::FanService::spawn(cfg.fans.clone()).unwrap_or_else(|e| {
        eprintln!("failed to start fan control thread: {e}");
        std::process::exit(1);
    });

    let mut state = AppState::new(cfg.clone());
    state.refresh_all();
    state.init_leds();
    state.set_fan_health(fans.status().health);

    // A panic in the event loop is caught only long enough to hand the
    // fans back before it propagates; the process still exits non-zero
    // and systemd restarts it.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        event_loop(&mut state, &mut lcm, &cfg, &fans, &rx)
    }));
    fans.shutdown();
    match outcome {
        Ok(Ok(())) => syslog::info("stopping: signal received, fans handed back"),
        Ok(Err(e)) => {
            syslog::critical(&format!("{e}; exiting so systemd restarts the daemon"));
            std::process::exit(1);
        }
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Runs until SIGTERM/SIGINT (`Ok`) or until fan control can no longer be
/// trusted (`Err`).
fn event_loop(
    state: &mut AppState,
    lcm: &mut Lcm,
    cfg: &Config,
    fans: &fan::FanService,
    rx: &mpsc::Receiver<socket::SocketCommand>,
) -> Result<(), &'static str> {
    let serial_fd = lcm.as_raw_fd();

    while !shutdown::requested() {
        // Drain any socket commands that arrived since the last wakeup.
        // STATUS is handled here rather than forwarded into
        // `apply_socket_command`: building the report needs the fan
        // status too, which lives out here alongside `state`, not inside it.
        while let Ok(cmd) = rx.try_recv() {
            if let socket::SocketCommand::StatusRequest(resp_tx) = cmd {
                let _ = resp_tx.send(report::build(state, &fans.status(), cfg));
                continue;
            }
            state.apply_socket_command(cmd);
        }

        if cfg.sleep.enabled {
            state.set_schedule_sleep_wanted(cfg.sleep.contains(now_hhmm()));
        }

        if fans.has_died() {
            return Err("fan control thread died");
        }
        state.set_fan_health(fans.status().health);

        let effect = state.tick();
        apply_effect(effect, lcm);
        drain_pending_keys(state, lcm);

        // Wait for the next thing that could matter: a serial byte, or the
        // next timer deadline (scroll step / dwell / confirm timeout).
        // A conservative fixed ceiling keeps this simple while still being
        // effectively event-driven -- most of the time nothing is scrolling
        // and this sleeps the full interval.
        let mut pfd = libc::pollfd {
            fd: serial_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid pollfd for the duration of the call,
        // and the count (1) matches.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, POLL_INTERVAL_MS) };
        if rc > 0
            && pfd.revents & libc::POLLIN != 0
            && let Some(frame) = lcm.read_frame(Duration::from_millis(50))
        {
            if frame.is_unsolicited() {
                let _ = lcm.ack(frame.subcmd);
            }
            if let Some(key) = frame.key() {
                let effect = state.handle_key(key);
                apply_effect(effect, lcm);
                drain_pending_keys(state, lcm);
            }
        }
    }
    Ok(())
}

/// Dispatches button presses `Lcm::send_and_ack` had to queue (see its
/// doc) instead of discarding -- e.g. a press that arrived while a
/// `set_text` call was mid-flight waiting on its own ACK. Feeds each one
/// through `state.handle_key` exactly like the top-level `poll()` path does,
/// so it isn't lost until the MCU gets around to resending it.
fn drain_pending_keys(state: &mut AppState, lcm: &mut Lcm) {
    for frame in lcm.take_pending() {
        if let Some(key) = frame.key() {
            let effect = state.handle_key(key);
            apply_effect(effect, lcm);
        }
    }
}

fn apply_effect(effect: Effect, lcm: &mut Lcm) {
    match effect {
        Effect::Render(line0, line1) => {
            let _ = lcm.set_text(0, &line0, 0);
            let _ = lcm.set_text(1, &line1, 0);
        }
        Effect::RunAction(action) => run_action(action),
        Effect::None => {}
    }
}

fn run_action(action: Action) {
    match action {
        Action::Shutdown => {
            let _ = std::process::Command::new("systemctl")
                .arg("poweroff")
                .status();
        }
        Action::Restart => {
            let _ = std::process::Command::new("systemctl")
                .arg("reboot")
                .status();
        }
    }
}

/// Local wall-clock hour/minute (`[sleep].start`/`.end` are documented as
/// local time, see config.rs). Needs `libc::localtime_r` -- computing this
/// from `SystemTime`/`UNIX_EPOCH` directly gives UTC, not local time, which
/// silently shifted the sleep window by the system's UTC offset (e.g. 7
/// hours early on a Pacific-time box) with no error or indication anything
/// was wrong.
fn now_hhmm() -> (u32, u32) {
    // SAFETY: `time` accepts a null output pointer; `tm` is plain data for
    // which all-zeroes is valid, and both pointers passed to the reentrant
    // `localtime_r` are to locals that outlive the call.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const t, &raw mut tm);
        tm
    };
    // localtime_r yields 0-23 / 0-59; fall back to midnight if not.
    let field = |v: libc::c_int| u32::try_from(v).unwrap_or(0);
    (field(tm.tm_hour), field(tm.tm_min))
}

fn run_probe_command(cmd: &str, args: &[String]) {
    let mut lcm = match Lcm::open(LCM_DEVICE) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to open {LCM_DEVICE}: {e}");
            std::process::exit(1);
        }
    };

    match cmd {
        "init" => {
            let (ok1, ok2) = lcm.init().unwrap_or((false, false));
            println!("init: step1={ok1} step2={ok2}");
        }
        "settext" => {
            let line: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let text = args.get(3).cloned().unwrap_or_default();
            let ok = lcm.set_text(line, &text, 0).unwrap_or(false);
            println!("settext line={line} ok={ok}");
        }
        "listen" => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
            println!("listening for {secs}s (press panel buttons now)...");
            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline {
                if let Some(f) = lcm.read_frame(Duration::from_millis(500)) {
                    print!(
                        "<- opcode={:#04X} subcmd={:#04X} payload={:02X?} cksum_ok={}",
                        f.opcode, f.subcmd, f.payload, f.checksum_ok
                    );
                    if let Some(key) = f.key() {
                        print!("  => KEY {key:?}");
                    } else if f.is_unsolicited()
                        && f.subcmd == SUB_VERSION
                        && let [major, minor, patch, ..] = f.payload[..]
                    {
                        print!("  => MCU VERSION {major}.{minor}.{patch}");
                    }
                    if f.opcode == OP_COMMAND {
                        let _ = lcm.ack(f.subcmd);
                    }
                    println!();
                }
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        let cmdline: Vec<String> = std::iter::once("lcm-status")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        parse_args(&cmdline)
    }

    fn default_path() -> PathBuf {
        PathBuf::from(config::DEFAULT_CONFIG_PATH)
    }

    #[test]
    fn mistyped_subcommand_is_rejected_not_run_as_a_daemon() {
        assert!(parse(&["stauts"]).is_err());
        assert!(parse(&["hal-tset"]).is_err());
    }

    #[test]
    fn daemon_forms() {
        assert_eq!(parse(&[]), Ok(Cli::Daemon(default_path())));
        assert_eq!(parse(&["daemon"]), Ok(Cli::Daemon(default_path())));
        assert_eq!(
            parse(&["daemon", "/tmp/x.toml"]),
            Ok(Cli::Daemon("/tmp/x.toml".into()))
        );
        // Legacy unit-file form.
        assert_eq!(
            parse(&["/etc/lcm-status.toml"]),
            Ok(Cli::Daemon("/etc/lcm-status.toml".into()))
        );
    }

    #[test]
    fn subcommands_take_an_optional_config_path() {
        assert_eq!(parse(&["status"]), Ok(Cli::Status(default_path())));
        assert_eq!(
            parse(&["check-config", "a.toml"]),
            Ok(Cli::CheckConfig("a.toml".into()))
        );
        assert!(parse(&["status", "a.toml", "extra"]).is_err());
    }

    #[test]
    fn fan_profile_flags_are_not_taken_as_the_path() {
        assert_eq!(
            parse(&["fan-profile", "--yes"]),
            Ok(Cli::FanProfile {
                config: default_path(),
                assume_yes: true
            })
        );
        assert_eq!(
            parse(&["fan-profile", "-y", "/tmp/c.toml"]),
            Ok(Cli::FanProfile {
                config: "/tmp/c.toml".into(),
                assume_yes: true
            })
        );
    }
}
