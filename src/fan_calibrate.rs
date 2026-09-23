//! `lcm-status fan-profile` -- the discovery/calibration counterpart to
//! lm-sensors' `pwmconfig`. Run by hand, not by the daemon: enumerates
//! every hwmon temp sensor and pwm output on the box, reports which
//! sensors actually look connected, sweeps each pwm output to find which
//! (if any) fan responds and what its real min-start/min-stop PWM values
//! are, and prints a `[[fans]]` config block you can review and drop into
//! `lcm-status.toml`. Doesn't write your config for you -- what curve
//! temps to use and which discovered sensors should feed which fan are
//! judgment calls, not something a PWM sweep can determine.
//!
//! Takes over the pwm outputs it touches, so it stops `lcm-status.service`
//! first (if running) and restarts it when done, and restores every pwm
//! output's original enable-mode/value before exiting -- this is a
//! diagnostic pass, not meant to leave anything running off its own
//! commanded values.

use crate::hal::{read_temp_input, temp_inputs};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

pub fn run(config_path: &Path) {
    let args: Vec<String> = std::env::args().collect();
    let assume_yes = args.iter().any(|a| a == "--yes" || a == "-y");

    if !running_as_root() {
        eprintln!("must run as root (reads/writes hwmon pwm files)");
        std::process::exit(1);
    }

    println!("== lcm-status fan-profile ==");
    println!("Discovery/calibration pass -- see src/fan_calibrate.rs for what this does.");
    println!();

    print_sensor_inventory();

    let candidates = find_pwm_candidates();
    if candidates.is_empty() {
        println!("No pwm-capable hwmon chips found. Nothing to calibrate.");
        return;
    }

    println!("== PWM outputs found ==");
    for c in &candidates {
        println!("  {} pwm{} (chip at {})", c.chip_name, c.pwm_index, c.hwmon);
    }
    println!();
    println!("About to sweep {} pwm output(s) to find which fans respond and their", candidates.len());
    println!("real min-start/min-stop PWM. Each fan will briefly ramp through its full");
    println!("range (roughly 30-60s per output) -- normal, expected, not a malfunction.");
    println!();

    if !assume_yes && !confirm("Continue? [y/N] ") {
        println!("Aborted, nothing changed.");
        return;
    }

    let daemon_was_active = systemctl_is_active("lcm-status.service");
    if daemon_was_active {
        println!("Stopping lcm-status.service for the duration of this sweep...");
        let _ = std::process::Command::new("systemctl").args(["stop", "lcm-status.service"]).status();
    }

    let mut results = Vec::new();
    for c in &candidates {
        println!();
        println!("--- {} pwm{} ---", c.chip_name, c.pwm_index);
        results.push(calibrate_one(c));
    }

    if daemon_was_active {
        println!();
        println!("Restarting lcm-status.service...");
        let _ = std::process::Command::new("systemctl").args(["start", "lcm-status.service"]).status();
    }

    println!();
    println!("== Results ==");
    let mut found_any = false;
    for r in &results {
        match r {
            CalibrationResult::Found { candidate, fan_index, min_start_pwm, min_stop_pwm, max_pwm_rpm } => {
                found_any = true;
                println!(
                    "  {} pwm{} -> fan{}: min_start_pwm={min_start_pwm} min_stop_pwm={min_stop_pwm} (max ~{max_pwm_rpm:.0} RPM)",
                    candidate.chip_name, candidate.pwm_index, fan_index
                );
            }
            CalibrationResult::NoFanDetected { candidate } => {
                println!("  {} pwm{} -> no fan responded, skipping", candidate.chip_name, candidate.pwm_index);
            }
        }
    }

    if !found_any {
        println!();
        println!("No fans detected on any pwm output. Nothing to propose.");
        return;
    }

    println!();
    println!("== Proposed config (review before using -- see comments) ==");
    println!();
    for (i, r) in results.iter().enumerate() {
        if let CalibrationResult::Found { candidate, fan_index, min_start_pwm, min_stop_pwm, .. } = r {
            print_proposed_toml(i, candidate, *fan_index, *min_start_pwm, *min_stop_pwm);
        }
    }
    println!("# min_temp_c/max_temp_c above are this project's previous defaults, not");
    println!("# something a PWM sweep can determine -- adjust for your own thermal");
    println!("# preferences. Pick `sensors` from the inventory printed above (chip name");
    println!("# is enough for most; add `label`/`input` to narrow a multi-sensor chip).");
    println!();
    println!("Append the block(s) above to {} under `[[fans]]`,", config_path.display());
    println!("or replace its existing `[[fans]]` entries, then restart lcm-status.service.");
}

// --- Sensor inventory (read-only, no hardware manipulation) --------------

fn print_sensor_inventory() {
    println!("== Temp sensors found ==");
    let mut any = false;
    for (hwmon, chip) in all_hwmon() {
        if chip.is_empty() {
            continue;
        }
        for input in temp_inputs(&hwmon) {
            any = true;
            let label = std::fs::read_to_string(format!("{hwmon}/{input}_label"))
                .map(|s| s.trim().to_string())
                .ok()
                .filter(|s| !s.is_empty());
            let raw = std::fs::read_to_string(format!("{hwmon}/{input}_input"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            let connected = read_temp_input(&hwmon, &input);
            let label_str = label.map(|l| format!(" \"{l}\"")).unwrap_or_default();
            match connected {
                Some(t) => println!("  [connected]    {chip} {input}{label_str}: {t:.1}C"),
                None => {
                    let raw_str = raw.map(|r| format!("{:.1}C raw", r as f32 / 1000.0)).unwrap_or_else(|| "unreadable".to_string());
                    println!("  [unconnected]  {chip} {input}{label_str}: {raw_str} (fault flag set, or outside plausible range)");
                }
            }
        }
    }
    if !any {
        println!("  (none found)");
    }
    println!();
}

fn all_hwmon() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for e in entries.flatten() {
            let path = e.path().to_string_lossy().to_string();
            let name = std::fs::read_to_string(format!("{path}/name")).unwrap_or_default().trim().to_string();
            out.push((path, name));
        }
    }
    out.sort();
    out
}

// --- PWM discovery + calibration ------------------------------------------

struct PwmCandidate {
    hwmon: String,
    chip_name: String,
    pwm_index: u32,
}

enum CalibrationResult<'a> {
    Found { candidate: &'a PwmCandidate, fan_index: u32, min_start_pwm: u8, min_stop_pwm: u8, max_pwm_rpm: f32 },
    NoFanDetected { candidate: &'a PwmCandidate },
}

fn find_pwm_candidates() -> Vec<PwmCandidate> {
    let mut out = Vec::new();
    for (hwmon, chip) in all_hwmon() {
        if chip.is_empty() {
            continue;
        }
        for idx in pwm_indices(&hwmon) {
            out.push(PwmCandidate { hwmon: hwmon.clone(), chip_name: chip.clone(), pwm_index: idx });
        }
    }
    out
}

fn pwm_indices(hwmon: &str) -> Vec<u32> {
    let mut v = Vec::new();
    if let Ok(entries) = std::fs::read_dir(hwmon) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(n) = name.strip_prefix("pwm") {
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(idx) = n.parse() {
                        v.push(idx);
                    }
                }
            }
        }
    }
    v.sort();
    v
}

fn fan_indices(hwmon: &str) -> Vec<u32> {
    let mut v = Vec::new();
    if let Ok(entries) = std::fs::read_dir(hwmon) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("fan") {
                if let Some(n) = rest.strip_suffix("_input") {
                    if let Ok(idx) = n.parse() {
                        v.push(idx);
                    }
                }
            }
        }
    }
    v.sort();
    v
}

fn read_fan_rpm(hwmon: &str, idx: u32) -> f32 {
    std::fs::read_to_string(format!("{hwmon}/fan{idx}_input"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.0)
}

fn set_pwm(hwmon: &str, idx: u32, value: u8) {
    let _ = std::fs::write(format!("{hwmon}/pwm{idx}_enable"), "1");
    let _ = std::fs::write(format!("{hwmon}/pwm{idx}"), value.to_string());
}

fn calibrate_one(c: &PwmCandidate) -> CalibrationResult<'_> {
    let fans = fan_indices(&c.hwmon);

    // Save original state to restore afterward, whatever we find.
    let orig_enable = std::fs::read_to_string(format!("{}/pwm{}_enable", c.hwmon, c.pwm_index)).ok();
    let orig_pwm = std::fs::read_to_string(format!("{}/pwm{}", c.hwmon, c.pwm_index)).ok();
    let restore = || {
        if let Some(v) = &orig_pwm {
            let _ = std::fs::write(format!("{}/pwm{}", c.hwmon, c.pwm_index), v.trim());
        }
        if let Some(v) = &orig_enable {
            let _ = std::fs::write(format!("{}/pwm{}_enable", c.hwmon, c.pwm_index), v.trim());
        }
    };

    if fans.is_empty() {
        println!("  no tachometer inputs on this chip at all -- can't confirm a fan responds, skipping");
        return CalibrationResult::NoFanDetected { candidate: c };
    }

    // A single "ramp to max, see what's nonzero" snapshot isn't enough to
    // prove *this* pwm output caused anything -- a chip can expose more
    // pwmN/fanN sysfs files than it has real, wired fan headers (found the
    // hard way: this board's it8625 exposes pwm1..pwm6, but only pwm1 is
    // physically connected to anything; pwm2..pwm6 are no-ops, yet the
    // naive before/after check "found" a response on all of them, because
    // fan1 was still drifting toward steady-state from the *previous*
    // candidate's test while we happened to be prodding an inert pwm).
    // Proving causation instead: max -> low -> max, and only credit a fan
    // whose RPM actually drops at the low point and recovers afterward --
    // drift from an unrelated cause doesn't reverse itself on command like
    // that.
    println!("  probing for a real response (max -> low -> max, not just nonzero RPM)...");
    set_pwm(&c.hwmon, c.pwm_index, 255);
    sleep(Duration::from_secs(2));
    let hi1: Vec<(u32, f32)> = fans.iter().map(|&i| (i, read_fan_rpm(&c.hwmon, i))).collect();

    set_pwm(&c.hwmon, c.pwm_index, 20);
    sleep(Duration::from_secs(3));
    let lo: Vec<(u32, f32)> = fans.iter().map(|&i| (i, read_fan_rpm(&c.hwmon, i))).collect();

    set_pwm(&c.hwmon, c.pwm_index, 255);
    sleep(Duration::from_secs(3));
    let hi2: Vec<(u32, f32)> = fans.iter().map(|&i| (i, read_fan_rpm(&c.hwmon, i))).collect();

    let get = |v: &[(u32, f32)], i: u32| v.iter().find(|(idx, _)| *idx == i).map(|&(_, v)| v).unwrap_or(0.0);
    let best = fans
        .iter()
        .filter_map(|&i| {
            let (h1, l, h2) = (get(&hi1, i), get(&lo, i), get(&hi2, i));
            let peak = h1.max(h2);
            if peak <= 0.0 {
                return None; // never spun at all
            }
            // Real control: drops to well under peak at the low point.
            // Threshold is generous (70%) since some fans coast a lot on
            // momentum even at pwm=20 -- this only needs to reject "didn't
            // move at all", not measure precisely.
            if l < peak * 0.7 {
                Some((i, h2, peak - l))
            } else {
                None
            }
        })
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap());

    let Some((fan_index, max_rpm, _delta)) = best else {
        println!("  no fan showed a real response to this pwm output (RPM didn't drop when lowered) -- skipping");
        restore();
        return CalibrationResult::NoFanDetected { candidate: c };
    };
    println!("  fan{fan_index} responds (confirmed causally, ~{max_rpm:.0} RPM at pwm=255)");

    println!("  ramping down to find min_stop_pwm (where it stalls)...");
    let mut min_stop_pwm: u8 = 255;
    let mut pwm: i32 = 255;
    let mut actually_stalled = false;
    loop {
        pwm -= 15;
        if pwm < 0 {
            pwm = 0;
        }
        set_pwm(&c.hwmon, c.pwm_index, pwm as u8);
        sleep(Duration::from_millis(1500));
        let rpm = read_fan_rpm(&c.hwmon, fan_index);
        if rpm <= 0.0 {
            actually_stalled = true;
            break;
        }
        min_stop_pwm = pwm as u8;
        if pwm == 0 {
            break; // never stalls even at 0 -- see below, handled explicitly
        }
    }

    let min_start_pwm: u8;
    if actually_stalled {
        println!("  stalled below pwm={pwm} -- min_stop_pwm candidate: {min_stop_pwm}");
        println!("  ramping up from stopped to find min_start_pwm (where it restarts)...");
        let mut up: i32 = pwm;
        min_start_pwm = loop {
            up += 10;
            if up > 255 {
                up = 255;
            }
            set_pwm(&c.hwmon, c.pwm_index, up as u8);
            sleep(Duration::from_millis(1500));
            let rpm = read_fan_rpm(&c.hwmon, fan_index);
            if rpm > 0.0 || up == 255 {
                break up as u8;
            }
        };
        println!("  restarts at pwm={min_start_pwm} -- min_start_pwm candidate: {min_start_pwm}");
    } else {
        // Never stalled even at pwm=0 -- some fans coast/free-spin at any
        // commanded duty on this hardware (observed on this board's own
        // fan1). There's no real "start from stopped" to measure, so
        // min_start_pwm isn't meaningful; report min_stop_pwm as both.
        // IMPORTANT: this does NOT mean 0 is actually a *good* PWM to run
        // at long-term -- a fan that never technically reaches 0 RPM can
        // still be unstable, noisy, or wear unevenly at very low duty.
        // This board's original hand-tuned curve used min_pwm=50 despite
        // the fan technically never stopping either; treat a low empirical
        // min_stop_pwm here as a floor to sanity-check against your own
        // judgment, not a value to use as-is.
        min_start_pwm = min_stop_pwm;
        println!("  never stalled, even at pwm=0 -- this fan free-spins at any commanded");
        println!("  duty on this hardware. min_start_pwm isn't meaningful here; reporting");
        println!("  min_stop_pwm={min_stop_pwm} as a floor only -- sanity-check this against");
        println!("  noise/stability at low speed, don't just take the empirical zero at face value.");
    }

    restore();
    CalibrationResult::Found {
        candidate: c,
        fan_index,
        min_start_pwm,
        min_stop_pwm: min_stop_pwm.max(1), // 0 would mean "never runs" as a ramp floor; floor at 1
        max_pwm_rpm: max_rpm,
    }
}

fn print_proposed_toml(index: usize, c: &PwmCandidate, fan_index: u32, min_start_pwm: u8, min_stop_pwm: u8) {
    let name = if index == 0 { "chassis".to_string() } else { format!("fan{index}") };
    println!("[[fans]]");
    println!("name = \"{name}\"");
    println!("pwm_chip = \"{}\"", c.chip_name);
    println!("pwm_index = {}", c.pwm_index);
    println!("fan_index = {fan_index}");
    println!("update_secs = 1");
    println!("min_temp_c = 45.0");
    println!("max_temp_c = 90.0");
    println!("min_start_pwm = {min_start_pwm}");
    println!("min_stop_pwm = {min_stop_pwm}");
    println!("min_pwm = {min_stop_pwm}");
    println!("max_pwm = 255");
    println!("sensors = []  # fill in from the sensor inventory above, e.g.:");
    println!("# [[fans.sensors]]");
    println!("# chip = \"coretemp\"");
    println!("# label = \"Package\"");
    println!();
}

// --- small utilities --------------------------------------------------

fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn systemctl_is_active(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}
