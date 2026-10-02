//! Data gathering for each status screen. Every function is a plain,
//! synchronous, best-effort snapshot -- callers decide how often to call
//! these (see the refresh-on-display design), not this module.

use crate::config::{Config, NetworkConfig, ScreenTemplate, TempUnits};
use crate::led::BayState;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Two 16-char-window-ready lines. Line contents may be longer than 16
/// chars; the display layer handles truncation/scrolling.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub line0: String,
    pub line1: String,
}

/// Renders one `[templates.*]` entry into a `Screen`. `vars` must cover
/// that kind's `TemplatesConfig::*_VARS` list.
fn render(tpl: &ScreenTemplate, vars: &[(&str, &str)]) -> Screen {
    Screen {
        line0: crate::template::render(&tpl.line0, vars),
        line1: crate::template::render(&tpl.line1, vars),
    }
}

/// Upper bound on any one external command. These all run on the main
/// event loop, and some can block indefinitely: `zpool` on a pool with
/// suspended I/O, `docker` with a wedged daemon, `smartctl` on a dying
/// drive.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    run_with_timeout(cmd, args, COMMAND_TIMEOUT)
}

/// Runs `cmd`, returning its trimmed stdout if it exits successfully
/// within `timeout`. On timeout the child is killed and reaped on a
/// background thread rather than waited on here: a process stuck in
/// uninterruptible I/O ignores SIGKILL until the I/O completes, so
/// waiting for it would just move the hang.
fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Drain stdout concurrently so a chatty child can't fill the pipe and
    // block before exiting.
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                crate::syslog::warning(&format!(
                    "`{cmd} {}` timed out after {}s, killed",
                    args.join(" "),
                    timeout.as_secs()
                ));
                let _ = child.kill();
                thread::spawn(move || child.wait());
                return None;
            }
            Err(_) => return None,
        }
    };

    let stdout = reader.join().ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&stdout).trim().to_string())
}

/// Every *physical* interface with a global-scope IPv4 -- i.e. actually
/// brought into service in TrueNAS's own network config, as opposed to
/// merely existing as hardware (`physical_nics()`, below, returns those
/// too). Filters out bridges/veth/docker/incus interfaces via
/// /sys/class/net/{iface}/device -- only real hardware NICs have that
/// symlink, which is a more reliable filter than guessing at name
/// prefixes (br-, veth, docker0, incusbr0, ... is an open-ended list that
/// will never fully keep up with every virtual interface naming scheme).
///
/// This distinction matters for more than the network screen: a NIC that
/// exists but was never assigned an address (e.g. an expansion card
/// that's physically installed but deliberately not cabled/configured
/// yet) reports link-down forever, same as a real outage would -- but
/// it's not a fault, since nothing ever expected it to be connected. See
/// `state::recompute_status_led`, which uses this (not `physical_nics()`)
/// for exactly that reason -- found live when the status LED sat amber
/// with every actual health check green, because the AQC113 card (present
/// in hardware, never configured with an IP) was being treated the same
/// as a real link failure on the NICs that matter.
pub fn configured_nics() -> Vec<String> {
    let mut nics: Vec<String> = ip_by_iface()
        .into_keys()
        .filter(|iface| is_physical(iface))
        .collect();
    nics.sort();
    nics
}

/// Only real hardware NICs have a `device` link -- see `configured_nics`.
fn is_physical(iface: &str) -> bool {
    Path::new(&format!("/sys/class/net/{iface}/device")).exists()
}

/// Which interfaces' link state feeds the status LED: the configured
/// `monitored_nics`, or (if that's empty) whatever is currently
/// `configured_nics()`.
pub fn monitored_nics(net: &NetworkConfig) -> Vec<String> {
    if net.monitored_nics.is_empty() {
        configured_nics()
    } else {
        net.monitored_nics.clone()
    }
}

/// Every interface with a global-scope IPv4, mapped to that address --
/// the raw data `configured_nics()` (names only) and `network()` (display
/// screens) both build on, and also used directly by `report::build` for
/// the `status` request. Extracted so there's one parser for `ip -4 -o
/// addr show scope global`, not three.
pub fn ip_by_iface() -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(text) = run("ip", &["-4", "-o", "addr", "show", "scope", "global"]) {
        for line in text.lines() {
            // e.g. "2: eth0    inet 192.168.1.196/24 brd ... scope global ..."
            let mut f = line.split_whitespace();
            let Some(iface) = f.nth(1).map(|s| s.trim_end_matches(':').to_string()) else {
                continue;
            };
            let Some(ip) = line
                .split_whitespace()
                .find(|tok| tok.contains('/') && tok.chars().next().unwrap_or(' ').is_ascii_digit())
                .map(|tok| tok.split('/').next().unwrap_or("").to_string())
            else {
                continue;
            };
            out.insert(iface, ip);
        }
    }
    out
}

/// `"connected, no IP"` (link up, nothing configured) or `"disconnected"`
/// (no carrier) for an interface with no address -- see `network()`'s doc
/// comment for why that distinction is worth making at all.
pub fn nic_link_text(iface: &str) -> String {
    if carrier_up(iface) {
        "connected, no IP".to_string()
    } else {
        "disconnected".to_string()
    }
}

/// Kernel-reported carrier: `Some(true)` up, `Some(false)` down, `None`
/// unreadable (e.g. the interface is administratively down).
fn carrier(iface: &str) -> Option<bool> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{iface}/carrier")).ok()?;
    Some(s.trim() == "1")
}

fn carrier_up(iface: &str) -> bool {
    carrier(iface) == Some(true)
}

/// True only for a *confirmed* down link -- unreadable doesn't count, so
/// an interface that can't report carrier never raises a network alarm.
pub fn link_is_down(iface: &str) -> bool {
    carrier(iface) == Some(false)
}

/// One screen per *every* physical NIC (`physical_nics()`, not just
/// `configured_nics()`), so a card that's plugged in but never assigned an
/// address -- or one with no cable at all -- is still visible here rather
/// than silently absent. Line 1 is the IP if it has one, otherwise
/// `"connected, no IP"` (link up, nothing configured) or `"disconnected"`
/// (no carrier) -- distinguishing those two matters: the first says "go
/// configure this in Network settings", the second says "check the
/// cable", and before this they looked identical (both just missing from
/// the screen).
pub fn network(tpl: &ScreenTemplate) -> Vec<Screen> {
    let with_ip = ip_by_iface();

    let screens: Vec<Screen> = physical_nics()
        .into_iter()
        .map(|iface| {
            let ip = with_ip.get(&iface).map_or("", String::as_str);
            let link = if carrier_up(&iface) { "up" } else { "down" };
            let ip_or_status = if ip.is_empty() {
                nic_link_text(&iface)
            } else {
                ip.to_string()
            };
            render(
                tpl,
                &[
                    ("iface", &iface),
                    ("ip", ip),
                    ("link", link),
                    ("ip_or_status", &ip_or_status),
                ],
            )
        })
        .collect();

    if screens.is_empty() {
        vec![Screen {
            line0: "NETWORK".into(),
            line1: "no interfaces".into(),
        }]
    } else {
        screens
    }
}

/// Every physical NIC name (same /sys/class/net/{iface}/device filter as
/// `network()`), regardless of whether it currently has an address -- used
/// for per-port LED convention (`nic_mode`, night mode), which makes sense
/// to apply to hardware that exists whether or not it's in service. NOT
/// used for "is the network down" fault detection -- see `configured_nics`
/// for that, and why the distinction matters.
pub fn physical_nics() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_physical(name))
        .collect()
}

/// One row of `zpool list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    pub size: String,
    pub alloc: String,
    pub free: String,
    pub cap: String,
    pub health: String,
}

/// Every pool, from one `zpool list` call -- shared by the pool screens,
/// pool health monitoring and the status LED. `None` if `zpool` itself
/// failed (distinct from "no pools").
pub fn pools() -> Option<Vec<Pool>> {
    run(
        "zpool",
        &["list", "-H", "-o", "name,size,alloc,free,capacity,health"],
    )
    .map(|out| parse_zpool_list(&out))
}

fn parse_zpool_list(out: &str) -> Vec<Pool> {
    out.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut f = line.split_whitespace().map(String::from);
            let mut next = || f.next().unwrap_or_else(|| "?".to_string());
            Pool {
                name: next(),
                size: next(),
                alloc: next(),
                free: next(),
                cap: next(),
                health: next(),
            }
        })
        .collect()
}

/// Storage + RAID health merged: one screen per pool.
pub fn pool_screens(tpl: &ScreenTemplate, pools: Option<&[Pool]>) -> Vec<Screen> {
    let Some(pools) = pools else {
        return vec![Screen {
            line0: "STORAGE".into(),
            line1: "zpool unavailable".into(),
        }];
    };
    pools
        .iter()
        .map(|p| {
            render(
                tpl,
                &[
                    ("name", &p.name),
                    ("size", &p.size),
                    ("alloc", &p.alloc),
                    ("free", &p.free),
                    ("cap", &p.cap),
                    ("health", &p.health),
                ],
            )
        })
        .collect()
}

fn list_disks() -> Vec<String> {
    run("lsblk", &["-d", "-n", "-o", "NAME,TYPE"])
        .map(|out| {
            out.lines()
                .filter(|l| l.trim_end().ends_with("disk"))
                .filter_map(|l| l.split_whitespace().next().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// `smartctl -H -n standby` verdict for one disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartStatus {
    Passed,
    Failed,
    /// Spun down; `-n standby` declined to wake it to ask.
    Standby,
    /// smartctl ran but said neither PASSED nor FAILED.
    Unknown,
    /// smartctl failed or isn't installed.
    Unavailable,
}

impl SmartStatus {
    fn parse(out: &str) -> Self {
        if out.contains("in STANDBY") {
            SmartStatus::Standby
        } else if out.contains("PASSED") {
            SmartStatus::Passed
        } else if out.contains("FAILED") {
            SmartStatus::Failed
        } else {
            SmartStatus::Unknown
        }
    }

    fn label(self) -> &'static str {
        match self {
            SmartStatus::Passed => "PASSED",
            SmartStatus::Failed => "FAILED",
            SmartStatus::Standby => "STANDBY",
            SmartStatus::Unknown => "UNKNOWN",
            SmartStatus::Unavailable => "N/A",
        }
    }
}

/// One whole disk (not a partition).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Kernel name, e.g. "sda", "nvme0n1".
    pub name: String,
    /// Physical bay number for SATA disks (see `ata_port_for`); `None`
    /// for NVMe, which has no bay LED on this chassis.
    pub bay: Option<u32>,
    pub smart: SmartStatus,
}

/// Every disk with its bay and SMART verdict -- one `smartctl` call per
/// disk, shared by the HDD screens, bay LEDs and SMART monitoring.
pub fn disks() -> Vec<Disk> {
    list_disks()
        .into_iter()
        .map(|name| {
            let smart = run(
                "smartctl",
                &["-H", "-n", "standby", &format!("/dev/{name}")],
            )
            .map_or(SmartStatus::Unavailable, |out| SmartStatus::parse(&out));
            Disk {
                bay: ata_port_for(&name),
                name,
                smart,
            }
        })
        .collect()
}

/// Per-bay LED state from SMART (SATA bays only).
pub fn bay_led_states(disks: &[Disk]) -> Vec<(u32, BayState)> {
    disks
        .iter()
        .filter_map(|d| {
            let state = match d.smart {
                SmartStatus::Standby => BayState::Standby,
                SmartStatus::Failed => BayState::Failed,
                _ => BayState::Normal,
            };
            Some((d.bay?, state))
        })
        .collect()
}

/// HDD: SMART health + temperature together, one screen per drive. SATA
/// bay numbers come from the ATA port in `ID_PATH` (e.g. "...-ata-3..." ->
/// BAY3), which is a real, stable physical identifier -- confirmed to
/// match this board's ata1..4 <-> sata1..4 LED convention, not just
/// whatever order the kernel happened to enumerate sdX in.
pub fn hdd(cfg: &Config, disks: &[Disk]) -> Vec<Screen> {
    if disks.is_empty() {
        return vec![Screen {
            line0: "HDD".into(),
            line1: "no disks found".into(),
        }];
    }

    let bay_temps = drivetemps_by_ata_port();

    disks
        .iter()
        .map(|disk| {
            let d = &disk.name;
            let (label, temp) = match disk.bay {
                Some(bay) => (format!("BAY{bay} {d}"), bay_temps.get(&bay).copied()),
                None => (d.clone(), nvme_temp_for(d)),
            };
            let bay = disk.bay.map(|b| b.to_string()).unwrap_or_default();
            let temp = temp
                .map(|t| {
                    let (val, unit) = display_temp(t, cfg.temperature.units);
                    format!("{val:.0}{unit}")
                })
                .unwrap_or_default();

            render(
                &cfg.templates.hdd,
                &[
                    ("label", &label),
                    ("bay", &bay),
                    ("dev", d),
                    ("status", disk.smart.label()),
                    ("temp", &temp),
                ],
            )
        })
        .collect()
}

/// CPU temperature + the one real fan on this board (fan2/fan3 read a
/// constant 0 RPM with ALARM -- confirmed unpopulated headers, not failed
/// fans, so they're deliberately excluded rather than shown as an alert).
pub fn cpu_and_fan(cfg: &Config) -> Vec<Screen> {
    let mut screens = Vec::new();
    if let Some(cpu_c) = coretemp_package() {
        screens.push(cpu_screen(cpu_c, cfg));
    }
    if let Some(rpm) = fan1_rpm() {
        screens.push(render(&cfg.templates.fan, &[("rpm", &format!("{rpm:.0}"))]));
    }
    if screens.is_empty() {
        screens.push(Screen {
            line0: "TEMPERATURE".into(),
            line1: "no sensors found".into(),
        });
    }
    screens
}

/// Converts for display only -- thresholds are always compared in C.
fn display_temp(celsius: f32, units: TempUnits) -> (f32, &'static str) {
    match units {
        TempUnits::C => (celsius, "C"),
        TempUnits::F => (celsius * 9.0 / 5.0 + 32.0, "F"),
    }
}

fn cpu_screen(celsius: f32, cfg: &Config) -> Screen {
    let (val, unit) = display_temp(celsius, cfg.temperature.units);
    let warn = if celsius >= cfg.temperature.warn_threshold {
        " !"
    } else {
        ""
    };
    render(
        &cfg.templates.cpu,
        &[
            ("temp", &format!("{val:.0}")),
            ("unit", unit),
            ("warn", warn),
        ],
    )
}

/// Resolves a block device to its physical bay number via
/// `/sys/class/ata_port/ataX/port_no` -- the same stable, per-controller
/// source the LED driver's own bay triggers use. That driver computes the
/// LED name from `ap->port_no + 1` in kernel source (0-indexed internal
/// field), but the "+1" already happened before the value reached sysfs --
/// confirmed empirically: ata1's `port_no` reads back as 1, not 0. So the
/// sysfs value is already the human-facing bay number as-is.
/// Deliberately NOT the "ataN" name itself: that's a global counter across
/// every SATA controller in probe order, so it only happens to match bay
/// number today because the ASM1164 is the system's only controller. It
/// would silently shift if a second controller (e.g. a PCIe SATA card)
/// were ever added.
fn ata_port_for(dev_name: &str) -> Option<u32> {
    let out = run(
        "udevadm",
        &["info", "-q", "property", &format!("/dev/{dev_name}")],
    )?;
    let devpath = out.lines().find(|l| l.starts_with("DEVPATH="))?;
    let ata_node = devpath.split('/').find(|seg| seg.starts_with("ata"))?;
    std::fs::read_to_string(format!("/sys/class/ata_port/{ata_node}/port_no"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Matches an NVMe namespace block device (e.g. "nvme0n1") to its hwmon
/// controller ("nvme0") via the hwmon's `device` symlink target.
fn nvme_temp_for(dev_name: &str) -> Option<f32> {
    // "nvme0n1" -> "nvme0" (split at the last 'n', which introduces the namespace number)
    let controller = dev_name
        .rsplit_once('n')
        .map_or(dev_name, |(ctrl, _ns)| ctrl);
    // An hwmon whose `device` link can't be read is skipped, not treated
    // as the end of the search.
    let hwmon = glob_hwmon("nvme")?.into_iter().find(|hwmon| {
        std::fs::read_link(format!("{hwmon}/device"))
            .is_ok_and(|link| link.file_name().is_some_and(|n| n == controller))
    })?;
    read_sysfs_f32(&format!("{hwmon}/temp1_input"))
}

pub fn read_sysfs_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli_c| milli_c / 1000.0)
}

/// Every `tempN` input a hwmon instance exposes (base names only, e.g.
/// `["temp1", "temp2"]` -- callers append `_input`/`_label`/`_fault`
/// themselves). Used to enumerate a chip's sensors without hardcoding how
/// many it has.
pub fn temp_inputs(hwmon: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(hwmon) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix("_input")
                .filter(|n| n.starts_with("temp"))
                .map(std::string::ToString::to_string)
        })
        .collect();
    names.sort();
    names
}

/// Reads one `tempN_input`, treating it as "not connected" (`None`) rather
/// than a real value if either:
/// - the kernel itself says so (`tempN_fault` == "1", a real signal some
///   chips expose for an open/disconnected thermal diode), or
/// - the reading is outside any plausible temperature for hardware that's
///   actually there (deliberately generous bounds -- this is a last-resort
///   sanity net, not a warning threshold).
///
/// Exists because this board's `it8625` onboard `temp1`-`temp3` have no
/// diode wired to them at all and read a constant, wildly-out-of-range
/// value forever (see `asustor-platform-driver`'s CLAUDE.md) -- with
/// nothing filtering that out, a naive "read every temp this chip has"
/// sensor selector would let a permanently-disconnected input drag a fan
/// curve's `max()` up to full speed forever.
pub fn read_temp_input(hwmon: &str, input: &str) -> Option<f32> {
    const PLAUSIBLE_MIN_C: f32 = -20.0;
    const PLAUSIBLE_MAX_C: f32 = 125.0;
    if let Ok(s) = std::fs::read_to_string(format!("{hwmon}/{input}_fault"))
        && s.trim() == "1"
    {
        return None;
    }
    read_sysfs_f32(&format!("{hwmon}/{input}_input"))
        .filter(|&t| (PLAUSIBLE_MIN_C..=PLAUSIBLE_MAX_C).contains(&t))
}

/// All temp readings matching a `SensorSelector` (config.rs) across every
/// hwmon instance of the named chip -- e.g. every populated drive bay for
/// `chip = "drivetemp"`, with no need to enumerate them by name. Chips
/// with no matching (or currently unreadable/disconnected) input simply
/// contribute nothing, same as an empty drive bay having no hwmon instance
/// at all.
pub fn resolve_selector(sel: &crate::config::SensorSelector) -> Vec<f32> {
    let mut out = Vec::new();
    let Some(hwmons) = glob_hwmon(&sel.chip) else {
        return out;
    };
    for hwmon in hwmons {
        let inputs: Vec<String> = match &sel.input {
            Some(i) => vec![i.clone()],
            None => temp_inputs(&hwmon),
        };
        for input in inputs {
            if let Some(wanted_label) = &sel.label {
                let label =
                    std::fs::read_to_string(format!("{hwmon}/{input}_label")).unwrap_or_default();
                if !label.contains(wanted_label.as_str()) {
                    continue;
                }
            }
            if let Some(t) = read_temp_input(&hwmon, &input) {
                out.push(t);
            }
        }
    }
    out
}

/// Every hwmon instance on the box as (sysfs path, chip name) -- the base
/// enumeration `resolve_selector`'s `glob_hwmon` (chip-name-filtered),
/// `fan_calibrate`'s inventory, and `monitor`'s temperature sweep all
/// build on. Skips any hwmon with no readable `name` (shouldn't normally
/// happen, but a directory mid-teardown during a module reload could
/// transiently look that way).
pub fn all_hwmon() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path().to_string_lossy().to_string();
            let name = std::fs::read_to_string(format!("{path}/name"))
                .ok()?
                .trim()
                .to_string();
            (!name.is_empty()).then_some((path, name))
        })
        .collect();
    out.sort();
    out
}

/// Every temp sensor on the box that currently reads as connected (see
/// `read_temp_input`), labeled for logging -- e.g. `"coretemp temp1
/// \"Package id 0\""`. Used for general threshold monitoring
/// (`monitor::HealthMonitor`), deliberately not scoped to whatever a fan
/// curve happens to select: a sensor with nothing driving off it (this
/// board's AQC113 PHY/MAC temps, say) is still worth alerting on.
///
/// Returns (chip name, description for logging, value). Chip name is
/// returned separately (not just folded into the description) so callers
/// can match per-chip threshold overrides (`config::TempThresholdOverride`)
/// without re-parsing the description string.
pub fn all_connected_temps() -> Vec<(String, String, f32)> {
    let mut out = Vec::new();
    for (hwmon, chip) in all_hwmon() {
        for input in temp_inputs(&hwmon) {
            let Some(t) = read_temp_input(&hwmon, &input) else {
                continue;
            };
            let label = std::fs::read_to_string(format!("{hwmon}/{input}_label"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let desc = match &label {
                Some(l) => format!("{chip} {input} \"{l}\""),
                None => format!("{chip} {input}"),
            };
            out.push((chip.clone(), desc, t));
        }
    }
    out
}

pub fn coretemp_package() -> Option<f32> {
    for hwmon in glob_hwmon("coretemp")? {
        // A transiently unreadable instance is skipped, not the end of
        // the search.
        let Ok(entries) = std::fs::read_dir(&hwmon) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("temp")
                && name.ends_with("_label")
                && let Ok(label) = std::fs::read_to_string(entry.path())
                && label.trim().starts_with("Package")
            {
                let input = entry.path().to_string_lossy().replace("_label", "_input");
                return read_sysfs_f32(&input);
            }
        }
    }
    None
}

/// Maps real bay number (via `ata_port_for`, not hwmon enumeration order)
/// to that bay's drive temperature.
fn drivetemps_by_ata_port() -> HashMap<u32, f32> {
    let mut out = HashMap::new();
    let Some(dirs) = glob_hwmon("drivetemp") else {
        return out;
    };
    for hwmon in dirs {
        let Some(dev_name) = block_device_for_hwmon(&hwmon) else {
            continue;
        };
        let (Some(bay), Some(temp)) = (
            ata_port_for(&dev_name),
            read_sysfs_f32(&format!("{hwmon}/temp1_input")),
        ) else {
            continue;
        };
        out.insert(bay, temp);
    }
    out
}

/// Resolves a hwmon device's `device` symlink down to its block device
/// name, e.g. hwmon3 -> .../host0/target0:0:0/0:0:0:0 -> "sda".
fn block_device_for_hwmon(hwmon: &str) -> Option<String> {
    let device_path = std::fs::canonicalize(format!("{hwmon}/device")).ok()?;
    let block_dir = device_path.join("block");
    std::fs::read_dir(block_dir)
        .ok()?
        .flatten()
        .next()
        .map(|e| e.file_name().to_string_lossy().to_string())
}

/// fan1 only -- fan2/fan3 on this board's it8625 read a constant 0 RPM
/// with ALARM regardless of load, confirmed to be unpopulated headers
/// rather than failed fans (same story as the dead chassis temp probes).
fn fan1_rpm() -> Option<f32> {
    for hwmon in glob_hwmon("it8625")? {
        if let Some(rpm) = read_sysfs_raw_f32(&format!("{hwmon}/fan1_input"))
            && rpm > 0.0
        {
            return Some(rpm);
        }
    }
    None
}

pub fn read_sysfs_raw_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn glob_hwmon(name_prefix: &str) -> Option<Vec<String>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        if let Ok(name) = std::fs::read_to_string(path.join("name"))
            && name.trim().starts_with(name_prefix)
        {
            found.push(path.to_string_lossy().to_string());
        }
    }
    Some(found)
}

/// Docker: any container not "healthy" or plain "Up" (running, no healthcheck).
/// Returns one screen per problem container; empty if everything's fine.
pub fn docker_issues(ignore: &[String], tpl: &ScreenTemplate) -> Vec<Screen> {
    let out = run(
        "docker",
        &["ps", "-a", "--format", "{{.Names}}\t{{.Status}}"],
    );
    let Some(out) = out else {
        return Vec::new(); // docker not available -- treat as "nothing to report", not an error state
    };

    out.lines()
        .filter_map(|line| {
            let mut f = line.splitn(2, '\t');
            let name = f.next()?.trim();
            let status = f.next()?.trim();
            if ignore.iter().any(|i| i == name) {
                return None;
            }
            let ok =
                status.contains("(healthy)") || (status.starts_with("Up") && !status.contains('('));
            if ok {
                None
            } else {
                Some(render(tpl, &[("name", name), ("status", status)]))
            }
        })
        .collect()
}

/// This machine's hostname, for the chassis `LOCATE` screen -- the thing
/// that tells two identical boxes in a rack apart. `None` if it can't be
/// read (or isn't set).
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes for the whole
    // call; `gethostname` writes at most that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    // Truncation may leave no terminator; take everything up to one if any.
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zpool_list() {
        let pools = parse_zpool_list(
            "NVMe\t928G\t120G\t808G\t12%\tONLINE\nHDD\t43.6T\t20T\t23.6T\t45%\tDEGRADED\n",
        );
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[1].name, "HDD");
        assert_eq!(pools[1].cap, "45%");
        assert_eq!(pools[1].health, "DEGRADED");
        assert_eq!(parse_zpool_list("short\n")[0].health, "?");
    }

    #[test]
    fn parses_smartctl_verdicts() {
        let passed = "SMART overall-health self-assessment test result: PASSED";
        assert_eq!(SmartStatus::parse(passed), SmartStatus::Passed);
        let failed = "SMART overall-health self-assessment test result: FAILED!";
        assert_eq!(SmartStatus::parse(failed), SmartStatus::Failed);
        let standby = "Device is in STANDBY mode, exit(2)";
        assert_eq!(SmartStatus::parse(standby), SmartStatus::Standby);
        assert_eq!(SmartStatus::parse("???"), SmartStatus::Unknown);
    }

    #[test]
    fn bay_leds_cover_only_sata_bays() {
        let disk = |name: &str, bay, smart| Disk {
            name: name.into(),
            bay,
            smart,
        };
        let disks = [
            disk("sda", Some(1), SmartStatus::Passed),
            disk("sdb", Some(2), SmartStatus::Failed),
            disk("sdc", Some(3), SmartStatus::Standby),
            disk("nvme0n1", None, SmartStatus::Failed),
        ];
        assert_eq!(
            bay_led_states(&disks),
            vec![
                (1, BayState::Normal),
                (2, BayState::Failed),
                (3, BayState::Standby)
            ]
        );
    }

    #[test]
    fn run_returns_stdout_on_success() {
        assert_eq!(
            run_with_timeout("echo", &["hello"], Duration::from_secs(5)).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn run_returns_none_on_failure() {
        assert_eq!(run_with_timeout("false", &[], Duration::from_secs(5)), None);
        assert_eq!(
            run_with_timeout("/nonexistent/cmd", &[], Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn run_gives_up_on_a_hung_command() {
        let start = Instant::now();
        assert_eq!(
            run_with_timeout("sleep", &["30"], Duration::from_millis(200)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }
}
