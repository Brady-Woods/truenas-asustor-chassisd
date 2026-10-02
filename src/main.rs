mod config;
mod fan;
mod fan_calibrate;
mod hal;
mod led;
mod monitor;
mod power;
mod protocol;
mod report;
mod shutdown;
mod socket;
mod state;
mod syslog;
mod template;
mod wol;

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
       lcm-status locate [BAY] [--ttl SECS] [--off] [CONFIG]
                                          blink a bay's LEDs (or, with no BAY, the
                                          chassis') to find it; --ttl 0 = until --off
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
    /// `locate`: sends a `LOCATE` to the running daemon. `ttl_secs: None`
    /// leaves the TTL to the daemon's default.
    Locate {
        config: PathBuf,
        bay: Option<u32>,
        ttl_secs: Option<u64>,
        off: bool,
    },
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
    match rest.as_slice() {
        [] => config_arg(&[]).map(Cli::Daemon),
        ["daemon", tail @ ..] => config_arg(tail).map(Cli::Daemon),
        ["status", tail @ ..] => config_arg(tail).map(Cli::Status),
        ["locate", tail @ ..] => parse_locate_args(tail),
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

/// A subcommand's optional trailing config path.
fn config_arg(args: &[&str]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from(config::DEFAULT_CONFIG_PATH)),
        [path] if !path.starts_with('-') => Ok(PathBuf::from(path)),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

/// `locate [BAY] [--ttl SECS] [--off] [CONFIG]`, flags in any order. A
/// bare number is the bay; anything else positional is the config path.
fn parse_locate_args(args: &[&str]) -> Result<Cli, String> {
    let (mut bay, mut ttl_secs, mut off) = (None, None, false);
    let mut positional = Vec::new();
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        match arg {
            "--off" => off = true,
            "--ttl" => {
                let value = args.next().ok_or("--ttl needs a number of seconds")?;
                let secs = value
                    .parse()
                    .map_err(|_| format!("--ttl: '{value}' is not a number of seconds"))?;
                ttl_secs = Some(secs);
            }
            _ if bay.is_none() && arg.parse::<u32>().is_ok() => {
                bay = arg.parse().ok().filter(|&b| b > 0);
                if bay.is_none() {
                    return Err("bays are numbered from 1".into());
                }
            }
            _ => positional.push(arg),
        }
    }
    if off && ttl_secs.is_some() {
        return Err("--ttl makes no sense with --off".into());
    }
    Ok(Cli::Locate {
        config: config_arg(&positional)?,
        bay,
        ttl_secs,
        off,
    })
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
            println!("\npower schedule (as of now, this machine's timezone):");
            for line in
                power::Scheduler::new(&cfg.power_schedule).describe(power::now_epoch(), false)
            {
                println!("  {line}");
            }
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
        Cli::Status(path) => {
            print!(
                "{}",
                socket_request(&Config::load(&path).socket.path, "STATUS")
            );
        }
        Cli::Locate {
            config,
            bay,
            ttl_secs,
            off,
        } => {
            let request = socket::locate_request(bay, ttl_secs, off);
            socket_request(&Config::load(&config).socket.path, &request);
            println!("sent: {request}");
        }
        Cli::Daemon(path) => run_daemon(&path),
    }
    ExitCode::SUCCESS
}

/// Client side of the socket protocol, for `status` and `locate`: connect
/// to the running daemon's own socket, send one request line, and return
/// whatever it writes back (a `STATUS` report; nothing, for the
/// fire-and-forget commands). Exits the process on any error. `STATUS`
/// talks to whatever's actually running -- not a fresh one-shot snapshot
/// the way `hal-test` is -- so it reflects live accumulated state (a fan's
/// stall history, an active override) a brand new process invocation
/// couldn't know about.
fn socket_request(socket_path: &str, request: &str) -> String {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to connect to {socket_path}: {e} (is lcm-status running?)");
            std::process::exit(1);
        }
    };
    if let Err(e) = stream.write_all(format!("{request}\n").as_bytes()) {
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
    response
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

    // Blink patterns need ledtrig-timer and the NIC LEDs ledtrig-netdev;
    // neither is loaded by default on TrueNAS.
    led::ensure_trigger_modules();

    // Started before the first (slow, subprocess-heavy) refresh below so
    // fan control is never waiting on it.
    let fans = fan::FanService::spawn(cfg.fans.clone(), &cfg.temperature).unwrap_or_else(|e| {
        eprintln!("failed to start fan control thread: {e}");
        std::process::exit(1);
    });

    let mut state = AppState::new(cfg.clone());
    state.refresh_all();
    state.init_leds();
    state.set_fan_health(fans.status().health);

    let mut wol = wol::WolKeeper::new(&cfg.wol);
    wol.enforce();

    // Its first poll (top of the event loop) arms the RTC wake alarm; it
    // never fires anything scheduled before that first poll.
    let mut power = power::Scheduler::new(&cfg.power_schedule);
    if !power.rules().is_empty() {
        syslog::info(&format!(
            "power schedule: {} rule(s); {}",
            power.rules().len(),
            power.describe(power::now_epoch(), false).join("; ")
        ));
    }

    // A panic in the event loop is caught only long enough to hand the
    // fans back before it propagates; the process still exits non-zero
    // and systemd restarts it.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        event_loop(&mut state, &mut lcm, &cfg, &fans, &mut wol, &mut power, &rx)
    }));
    fans.shutdown();
    // Last chance before a shutdown powers the box off (systemd stops
    // this service on the way down), in case anything reset WOL since the
    // last periodic recheck, or the RTC alarm isn't right yet.
    wol.enforce();
    power.sync_rtc(power::now_epoch());
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
    wol: &mut wol::WolKeeper,
    power: &mut power::Scheduler,
    rx: &mpsc::Receiver<socket::SocketCommand>,
) -> Result<(), &'static str> {
    let serial_fd = lcm.as_raw_fd();

    while !shutdown::requested() {
        // Drain any socket commands that arrived since the last wakeup.
        // STATUS is handled here rather than forwarded into
        // `apply_socket_command`: building the report needs the fan
        // status and power schedule too, which live out here alongside
        // `state`, not inside it.
        while let Ok(cmd) = rx.try_recv() {
            if let socket::SocketCommand::StatusRequest(resp_tx) = cmd {
                let _ = resp_tx.send(report::build(state, &fans.status(), power, cfg));
                continue;
            }
            state.apply_socket_command(cmd);
        }

        if cfg.sleep.enabled {
            state.set_schedule_sleep_wanted(cfg.sleep.contains(now_hhmm()));
        }

        if let Some(due) = power.poll(power::now_epoch()) {
            syslog::warning(&format!(
                "power schedule: {} due; {} in {}s unless a front-panel button is pressed",
                due.label,
                if due.action == Action::Shutdown {
                    "powering off"
                } else {
                    "restarting"
                },
                due.countdown_secs
            ));
            state.start_countdown(due.action, due.countdown_secs, &due.label);
        }

        if fans.has_died() {
            return Err("fan control thread died");
        }
        state.set_fan_health(fans.status().health);
        wol.maybe_enforce();

        let effect = state.tick();
        apply_effect(effect, lcm, power);
        drain_pending_keys(state, lcm, power);

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
                apply_effect(effect, lcm, power);
                drain_pending_keys(state, lcm, power);
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
fn drain_pending_keys(state: &mut AppState, lcm: &mut Lcm, power: &mut power::Scheduler) {
    for frame in lcm.take_pending() {
        if let Some(key) = frame.key() {
            let effect = state.handle_key(key);
            apply_effect(effect, lcm, power);
        }
    }
}

fn apply_effect(effect: Effect, lcm: &mut Lcm, power: &mut power::Scheduler) {
    match effect {
        Effect::Render(line0, line1) => {
            let _ = lcm.set_text(0, &line0, 0);
            let _ = lcm.set_text(1, &line1, 0);
        }
        Effect::RunAction(action) => {
            // The wake alarm is normally already right; this closes the
            // window right after a power on time passes (the alarm went
            // off while running, the next one not set yet).
            power.sync_rtc(power::now_epoch());
            run_action(action);
        }
        Effect::None => {}
    }
}

/// Shutdown/restart, from the panel menu or the power schedule. The same
/// path TrueNAS's own UI ends up at (systemd), so nothing is skipped.
fn run_action(action: Action) {
    let verb = match action {
        Action::Shutdown => "poweroff",
        Action::Restart => "reboot",
    };
    match std::process::Command::new("systemctl").arg(verb).status() {
        Ok(status) if status.success() => {}
        Ok(status) => syslog::critical(&format!("systemctl {verb} failed ({status})")),
        Err(e) => syslog::critical(&format!("systemctl {verb} failed to run: {e}")),
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

    fn locate(config: PathBuf, bay: Option<u32>, ttl_secs: Option<u64>, off: bool) -> Cli {
        Cli::Locate {
            config,
            bay,
            ttl_secs,
            off,
        }
    }

    #[test]
    fn locate_forms() {
        assert_eq!(
            parse(&["locate"]),
            Ok(locate(default_path(), None, None, false))
        );
        assert_eq!(
            parse(&["locate", "2", "--ttl", "300"]),
            Ok(locate(default_path(), Some(2), Some(300), false))
        );
        assert_eq!(
            parse(&["locate", "--off", "3", "/tmp/c.toml"]),
            Ok(locate("/tmp/c.toml".into(), Some(3), None, true))
        );
        assert_eq!(
            parse(&["locate", "--ttl", "0"]),
            Ok(locate(default_path(), None, Some(0), false))
        );
    }

    #[test]
    fn bad_locate_arguments_are_rejected() {
        assert!(parse(&["locate", "0"]).is_err());
        assert!(parse(&["locate", "--ttl"]).is_err());
        assert!(parse(&["locate", "--ttl", "soon"]).is_err());
        assert!(parse(&["locate", "2", "--off", "--ttl", "5"]).is_err());
        assert!(parse(&["locate", "2", "--bogus"]).is_err());
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
