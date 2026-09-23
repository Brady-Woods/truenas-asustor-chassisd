//! Front-panel LED control, thin sysfs wrapper + the status-LED decision
//! logic. Patterns are exactly what's documented for this hardware's LED
//! class devices under /sys/class/leds -- see the asustord project's
//! LED-MODES.md for the reference this was built against.
//!
//! Deliberately NOT touched here: `disk_led_ready` (requires reloading the
//! shared kernel LED driver, which power-cycles the LCD as a side effect --
//! out of scope, left as an operator-set kernel module option).

use crate::socket::Level;
use serde::Deserialize;
use std::path::Path;

const LEDS: &str = "/sys/class/leds";

fn write_attr(led: &str, attr: &str, value: &str) {
    let _ = std::fs::write(format!("{LEDS}/{led}/{attr}"), value);
}

fn set_solid(led: &str, on: bool) {
    write_attr(led, "trigger", "none");
    write_attr(led, "brightness", if on { "1" } else { "0" });
}

fn set_blink(led: &str, on_ms: u32, off_ms: u32) {
    write_attr(led, "trigger", "timer");
    write_attr(led, "delay_on", &on_ms.to_string());
    write_attr(led, "delay_off", &off_ms.to_string());
}

/// The five documented status-LED patterns, worst-first so callers can just
/// pick the single most severe condition currently true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPattern {
    Ok,               // solid green
    /// Solid amber (green+red both on) -- "our addition" per the doc, not a
    /// factory pattern. Originally just "network down"; now the generic
    /// non-critical warning indicator (some monitored NICs down, a temp/fan
    /// warning, etc.) -- see `state::recompute_status_led`. Deliberately
    /// coarse: which specific thing tripped it is on the LCD/syslog, not
    /// encoded in the LED color.
    Warning,
    Degraded,         // green solid, red flashing 500/500 -- factory RAID-degraded pattern
    Failed,           // solid red -- factory "malfunction"
    CriticalFlashing, // red flashing 500/500, green off -- socket-driven `critical` level
}

pub fn set_status(pattern: StatusPattern) {
    match pattern {
        StatusPattern::Ok => {
            set_solid("green:status", true);
            write_attr("red:status", "trigger", "none");
            set_solid("red:status", false);
        }
        StatusPattern::Warning => {
            set_solid("green:status", true);
            set_solid("red:status", true);
        }
        StatusPattern::Degraded => {
            set_solid("green:status", true);
            set_blink("red:status", 500, 500);
        }
        StatusPattern::Failed => {
            set_solid("green:status", false);
            set_solid("red:status", true);
        }
        StatusPattern::CriticalFlashing => {
            set_solid("green:status", false);
            // 125ms was confirmed correctly applied at the driver level
            // (trigger=timer, delay_on/off=125) but too fast to read as
            // distinct on/off by eye -- 1s/1s gives an unambiguous full-on,
            // full-off cycle instead.
            set_blink("red:status", 1000, 1000);
        }
    }
}

/// Maps a socket-pushed severity level directly to a status pattern, for
/// when an active override should also drive the LED (info/warn don't
/// override the health-derived pattern; error/critical do).
pub fn pattern_for_level(level: Level) -> Option<StatusPattern> {
    match level {
        Level::Error => Some(StatusPattern::Failed),
        Level::Critical => Some(StatusPattern::CriticalFlashing),
        Level::Info | Level::Warn => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BayState {
    Normal,
    /// Confirmed SMART failure -- factory-documented pattern (solid red).
    Failed,
    /// An external error/critical alert named this bay, but it isn't
    /// (yet) a confirmed SMART failure -- flashes so it's visually
    /// distinct from a not-yet-elevated confirmed failure, but still
    /// clearly urgent, matching the status LED's critical flash rate.
    Alert,
    Standby,
}

/// Bay LEDs: red on for a failed drive; green slow-flash for standby/spun
/// down; otherwise hand activity back to the driver's own automatic
/// trigger rather than leaving our last override in place forever.
pub fn set_bay(bay: u32, state: BayState) {
    let green = format!("sata{bay}:green:disk");
    let red = format!("sata{bay}:red:disk");
    match state {
        BayState::Normal => {
            write_attr(&green, "trigger", &format!("asustor-sata{bay}"));
            set_solid(&red, false);
        }
        BayState::Failed => {
            write_attr(&green, "trigger", "none");
            set_solid(&green, false);
            set_solid(&red, true);
        }
        BayState::Alert => {
            write_attr(&green, "trigger", "none");
            set_solid(&green, false);
            set_blink(&red, 1000, 1000);
        }
        BayState::Standby => {
            set_blink(&green, 250, 9750);
            set_solid(&red, false);
        }
    }
}

/// Night mode: dark status/network/USB LEDs, bay green LEDs off -- but red
/// bay LEDs are deliberately left alone so a real failure still shows even
/// while "asleep", same principle as a critical alert waking the LCD.
pub fn enter_night_mode(nic_ifaces: &[String]) {
    write_attr("green:status", "trigger", "none");
    set_solid("green:status", false);
    write_attr("red:status", "trigger", "none");
    set_solid("red:status", false);

    for bay in 1..=4 {
        write_attr(&format!("sata{bay}:green:disk"), "trigger", "none");
        set_solid(&format!("sata{bay}:green:disk"), false);
        // sata{bay}:red:disk intentionally untouched.
    }

    set_solid("green:usb", false);
    write_attr("green:usb", "trigger", "none");

    for iface in nic_ifaces {
        for chip_led in [format!("{iface}-0::lan"), format!("{iface}-1::lan")] {
            for attr in ["link_10", "link_100", "link_1000", "link_2500", "rx", "tx"] {
                write_attr(&chip_led, attr, "0");
            }
        }
    }
}

/// Restores factory-automatic behavior for everything night mode touched.
pub fn exit_night_mode(nic_ifaces: &[String]) {
    write_attr("green:status", "trigger", "none");
    set_solid("green:status", true);
    write_attr("red:status", "trigger", "panic");
    set_solid("red:status", false);

    for bay in 1..=4 {
        write_attr(&format!("sata{bay}:green:disk"), "trigger", &format!("asustor-sata{bay}"));
    }

    write_attr("green:usb", "trigger", "asustor-front-usb");

    for iface in nic_ifaces {
        let a = format!("{iface}-0::lan");
        let b = format!("{iface}-1::lan");
        write_attr(&a, "link_2500", "1");
        for attr in ["link_10", "link_100", "link_1000"] {
            write_attr(&b, attr, "1");
        }
    }
}

/// NIC LED mode, applied once from config at startup (not health-driven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicLedMode {
    Link,
    Activity,
}

pub fn set_nic_mode(iface: &str, mode: NicLedMode) {
    let a = format!("{iface}-0::lan");
    let b = format!("{iface}-1::lan");
    match mode {
        NicLedMode::Link => {
            write_attr(&a, "trigger", "netdev");
            write_attr(&a, "link_2500", "1");
            write_attr(&a, "rx", "0");
            write_attr(&a, "tx", "0");
            write_attr(&b, "trigger", "netdev");
            for attr in ["link_10", "link_100", "link_1000"] {
                write_attr(&b, attr, "1");
            }
        }
        NicLedMode::Activity => {
            write_attr(&a, "trigger", "netdev");
            write_attr(&a, "link_2500", "0");
            write_attr(&a, "rx", "1");
            write_attr(&a, "tx", "1");
            write_attr(&b, "trigger", "netdev");
            for attr in ["link_10", "link_100", "link_1000"] {
                write_attr(&b, attr, "0");
            }
        }
    }
}

/// True if the interface's kernel-reported carrier is down. Used for the
/// "network disconnected" status LED pattern.
pub fn link_is_down(iface: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/carrier"))
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

pub fn ledtrig_timer_loaded() -> bool {
    Path::new("/sys/class/leds/red:status/delay_on").exists()
}

/// Checks for the same asustor-platform-driver (nas-deploy branch: main +
/// PRs #46/#47/#48) effects `deploy.sh` gates on before building at all.
/// This is defense-in-depth for the case where the binary gets started
/// some other way than `deploy.sh` (e.g. by hand, or a differently-set-up
/// systemd unit) -- everything here should already be guaranteed by the
/// time deploy.sh's own check has passed.
pub fn driver_present() -> bool {
    Path::new("/sys/class/leds/power:lcd").is_dir()
        && Path::new("/sys/class/leds/sata1:red:disk").is_dir()
        && Path::new("/sys/class/leds/green:status").is_dir()
}
