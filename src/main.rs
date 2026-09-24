mod config;
mod fan;
mod fan_calibrate;
mod hal;
mod led;
mod monitor;
mod protocol;
mod report;
mod socket;
mod state;
mod syslog;

use config::Config;
use protocol::{Key, Lcm, LCM_DEVICE};
use state::{Action, AppState, Effect};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");

    if cmd == "check-config" {
        let path = args.get(2).map(Path::new).unwrap_or(Path::new(config::DEFAULT_CONFIG_PATH));
        println!("{:#?}", Config::load(path));
        return;
    }

    if cmd == "hal-test" {
        let cfg = Config::default();
        println!("-- network --\n{:#?}", hal::network());
        println!("-- pools --\n{:#?}", hal::pools());
        println!("-- hdd --\n{:#?}", hal::hdd());
        println!("-- temperature/fan --\n{:#?}", hal::cpu_and_fan(&cfg));
        println!("-- docker issues --\n{:#?}", hal::docker_issues(&cfg.docker.ignore));
        return;
    }

    if cmd == "init" || cmd == "settext" || cmd == "listen" {
        return run_probe_command(cmd, &args);
    }

    if cmd == "fan-profile" {
        // Skip flags (e.g. --yes) when looking for a positional config
        // path, rather than blindly taking args[2] -- `--yes` would
        // otherwise get parsed as the path.
        let path = args
            .iter()
            .skip(2)
            .find(|a| !a.starts_with('-'))
            .map(|s| Path::new(s.as_str()))
            .unwrap_or(Path::new(config::DEFAULT_CONFIG_PATH));
        fan_calibrate::run(path);
        return;
    }

    if cmd == "status" {
        let path = args.get(2).map(Path::new).unwrap_or(Path::new(config::DEFAULT_CONFIG_PATH));
        let socket_path = Config::load(path).socket.path;
        request_status(&socket_path);
        return;
    }

    run_daemon(&args);
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

fn run_daemon(args: &[String]) {
    syslog::init();

    let cfg_path = args
        .get(1)
        .filter(|a| a.as_str() != "daemon")
        .map(String::as_str)
        .unwrap_or(config::DEFAULT_CONFIG_PATH);
    let cfg = Config::load(Path::new(cfg_path));
    syslog::info(&format!(
        "starting: {} fan(s) configured, temperature warn/critical at {:.0}C/{:.0}C",
        cfg.fans.iter().filter(|f| f.enabled).count(),
        cfg.temperature.warn_threshold,
        cfg.temperature.critical_threshold,
    ));

    let mut lcm = Lcm::open(&cfg.display.serial_device).unwrap_or_else(|e| {
        eprintln!("failed to open {}: {e}", cfg.display.serial_device);
        std::process::exit(1);
    });

    // Power-on init sequence, same bytes the original firmware sends.
    let _ = lcm.send_and_ack(0xF0, 0x11, &[0x01], Duration::from_millis(300));
    std::thread::sleep(Duration::from_millis(15));
    let _ = lcm.send_and_ack(0xF0, 0x22, &[0x00], Duration::from_millis(300));

    let (tx, rx) = mpsc::channel();
    let socket_path = cfg.socket.path.clone();
    let socket_group = cfg.socket.group.clone();
    if let Err(e) = socket::spawn(&socket_path, &socket_group, tx) {
        eprintln!("socket listener failed to start on {socket_path}: {e}");
    }

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
        let _ = std::process::Command::new("modprobe").arg("ledtrig-timer").status();
    }

    let mut state = AppState::new(cfg.clone());
    state.refresh_all();
    state.init_leds();

    // One controller per configured fan, each on its own cadence
    // (independent of the LCD rotation/scroll timing below). `tick()`
    // fires immediately on this first call (no `last_tick` yet), so the
    // curve applies from startup rather than waiting a full interval.
    let mut fans: Vec<fan::FanController> =
        cfg.fans.iter().cloned().map(fan::FanController::new).collect();
    for f in &mut fans {
        f.tick();
    }
    state.set_fan_health(worst_fan_health(&fans));

    let serial_fd = lcm.as_raw_fd();

    loop {
        // Drain any socket commands that arrived since the last wakeup.
        // STATUS is handled here rather than forwarded into
        // `apply_socket_command`: building the report needs `fans` too,
        // which lives out here alongside `state`, not inside it.
        while let Ok(cmd) = rx.try_recv() {
            if let socket::SocketCommand::StatusRequest(resp_tx) = cmd {
                let _ = resp_tx.send(report::build(&state, &fans, &cfg));
                continue;
            }
            state.apply_socket_command(cmd);
        }

        if cfg.sleep.enabled {
            state.set_schedule_sleep_wanted(in_sleep_window(&cfg.sleep.start, &cfg.sleep.end, now_hhmm()));
        }

        for f in &mut fans {
            f.tick();
        }
        state.set_fan_health(worst_fan_health(&fans));

        let effect = state.tick();
        apply_effect(effect, &mut lcm, &cfg);

        // Wait for the next thing that could matter: a serial byte, or the
        // next timer deadline (scroll step / dwell / confirm timeout).
        // A conservative fixed ceiling keeps this simple while still being
        // effectively event-driven -- most of the time nothing is scrolling
        // and this sleeps the full interval.
        let timeout_ms = next_wake_ms(&cfg);
        let mut pfd = libc::pollfd {
            fd: serial_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 && pfd.revents & libc::POLLIN != 0 {
            if let Ok(Some((opcode, subcmd, payload, ok))) = lcm.read_frame(Duration::from_millis(50)) {
                if ok && opcode == 0xF0 {
                    let _ = lcm.ack(subcmd);
                    if subcmd == 0x80 {
                        if let Some(&code) = payload.first() {
                            let key: Key = code.into();
                            let effect = state.handle_key(key);
                            apply_effect(effect, &mut lcm, &cfg);
                        }
                    }
                }
            }
        }
    }
}

fn apply_effect(effect: Effect, lcm: &mut Lcm, cfg: &Config) {
    match effect {
        Effect::Render(line0, line1) => {
            let _ = lcm.set_text(0, &line0, 0);
            let _ = lcm.set_text(1, &line1, 0);
        }
        Effect::RunAction(action) => run_action(action, cfg),
        Effect::SetLcdPower(on) => set_lcd_power(cfg, on),
        Effect::None => {}
    }
}

fn run_action(action: Action, _cfg: &Config) {
    match action {
        Action::Shutdown => {
            let _ = std::process::Command::new("systemctl").arg("poweroff").status();
        }
        Action::Restart => {
            let _ = std::process::Command::new("systemctl").arg("reboot").status();
        }
        Action::Eject => {
            // Placeholder: real implementation unmounts + powers down the
            // specific front-port device (PCI 00:14.0, root-hub port 2)
            // rather than any USB device system-wide.
            eprintln!("eject requested -- not yet wired to the actual front-port device");
        }
    }
}

fn set_lcd_power(cfg: &Config, on: bool) {
    let _ = std::fs::write(&cfg.sleep.lcd_power_path, if on { "1" } else { "0" });
}

fn now_hhmm() -> (u32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_of_day = now % 86400;
    ((secs_of_day / 3600) as u32, ((secs_of_day % 3600) / 60) as u32)
}

fn in_sleep_window(start: &str, end: &str, now: (u32, u32)) -> bool {
    let parse = |s: &str| -> Option<(u32, u32)> {
        let mut it = s.splitn(2, ':');
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    };
    let (Some(s), Some(e)) = (parse(start), parse(end)) else {
        return false;
    };
    let to_mins = |t: (u32, u32)| t.0 * 60 + t.1;
    let (s, e, n) = (to_mins(s), to_mins(e), to_mins(now));
    if s <= e {
        n >= s && n < e
    } else {
        n >= s || n < e // window wraps past midnight
    }
}

fn next_wake_ms(_cfg: &Config) -> i32 {
    // Deliberately short-and-simple: a fixed 100ms ceiling so scroll steps
    // and timers stay responsive without needing per-state deadline math
    // threaded through the poll() call. At idle (no scrolling, no pending
    // timers) this is the only cost -- one wakeup per 100ms is negligible.
    100
}

/// Worst current health across every configured fan -- fed into
/// `AppState::set_fan_health` each tick so the status LED can factor fan
/// trouble in. Lives here (not in `state.rs`) because `FanController`s are
/// a separate top-level value from `AppState` in this loop, not owned by it.
fn worst_fan_health(fans: &[fan::FanController]) -> socket::Level {
    fans.iter().map(|f| f.health_level()).max().unwrap_or(socket::Level::Info)
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
            let ok1 = lcm.send_and_ack(0xF0, 0x11, &[0x01], Duration::from_millis(300)).unwrap_or(false);
            std::thread::sleep(Duration::from_millis(15));
            let ok2 = lcm.send_and_ack(0xF0, 0x22, &[0x00], Duration::from_millis(300)).unwrap_or(false);
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
                match lcm.read_frame(Duration::from_millis(500)) {
                    Ok(Some((opcode, subcmd, payload, ok))) => {
                        print!("<- opcode={opcode:#04X} subcmd={subcmd:#04X} payload={payload:02X?} cksum_ok={ok}");
                        if opcode == 0xF0 {
                            if subcmd == 0x80 {
                                if let Some(&code) = payload.first() {
                                    let key: Key = code.into();
                                    print!("  => KEY {key:?} (code={code})");
                                }
                            } else if subcmd == 0x13 && payload.len() >= 3 {
                                print!("  => MCU VERSION {}.{}.{}", payload[0], payload[1], payload[2]);
                            }
                            let _ = lcm.ack(subcmd);
                        }
                        println!();
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("read error: {e}"),
                }
            }
        }
        _ => unreachable!(),
    }
}
