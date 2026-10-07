//! Data gathering for each status screen. Every function is a plain,
//! synchronous, best-effort snapshot -- callers decide how often to call
//! these (see the refresh-on-display design), not this module.

use crate::config::{Config, NetworkConfig, ScreenTemplate, TempUnits};
use crate::led::BayState;
use crate::protocol::poll_fd;
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
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

/// Most output kept from one command. Everything wanted here (zpool,
/// smartctl, docker, ip) is a few KiB; more than this means something is
/// wrong, and the command is abandoned rather than truncated mid-line.
const OUTPUT_MAX: usize = 1 << 20;
/// Children that were killed on timeout but haven't exited yet (a process
/// in uninterruptible I/O ignores SIGKILL until the I/O completes).
/// Kept so `run_with_timeout` can reap them later without a thread per
/// wedged child; past `UNREAPED_MAX` no new commands are started.
static UNREAPED: Mutex<Vec<Child>> = Mutex::new(Vec::new());
const UNREAPED_MAX: usize = 16;

/// Drops every child in `children` that has exited (reaping it).
fn reap(children: &mut Vec<Child>) {
    children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
}

/// Runs `cmd`, returning its trimmed stdout if it exits successfully
/// within `timeout`. On timeout the child is killed and reaped later
/// (see `UNREAPED`) rather than waited on here: a process stuck in
/// uninterruptible I/O ignores SIGKILL until the I/O completes, so
/// waiting for it would just move the hang.
///
/// Runs without helper threads: stdout is drained from this thread by
/// polling the pipe, so a wedged child (or a grandchild holding the pipe
/// open) can never strand a blocked reader.
fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    {
        let mut unreaped = UNREAPED.lock().unwrap_or_else(PoisonError::into_inner);
        reap(&mut unreaped);
        if unreaped.len() >= UNREAPED_MAX {
            crate::syslog::warning(&format!(
                "not running `{cmd}`: {} earlier commands are stuck and unkillable",
                unreaped.len()
            ));
            return None;
        }
    }

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let fd = stdout.as_raw_fd();
    let deadline = Instant::now() + timeout;
    let describe = || format!("`{cmd} {}`", args.join(" "));

    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut eof = false;
    let mut failure = None;
    while failure.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            failure = Some(format!("timed out after {}s", timeout.as_secs()));
        } else if eof {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return status
                        .success()
                        .then(|| String::from_utf8_lossy(&out).trim().to_string());
                }
                Ok(None) => thread::sleep(left.min(Duration::from_millis(10))),
                Err(_) => return None,
            }
        } else if poll_fd(fd, libc::POLLIN, left.min(Duration::from_millis(100))) {
            match stdout.read(&mut chunk) {
                Ok(0) => eof = true,
                Ok(n) => {
                    out.extend_from_slice(chunk.get(..n).unwrap_or_default());
                    if out.len() > OUTPUT_MAX {
                        failure = Some(format!("printed more than {OUTPUT_MAX} bytes"));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => eof = true,
            }
        }
    }

    crate::syslog::warning(&format!(
        "{} {}, killed",
        describe(),
        failure.unwrap_or_default()
    ));
    // A failed kill means the child already exited; either way keep it
    // until `try_wait` confirms it is reaped.
    if let Err(e) = child.kill() {
        crate::syslog::notice(&format!("{}: kill: {e}", describe()));
    }
    if !matches!(child.try_wait(), Ok(Some(_))) {
        UNREAPED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(child);
    }
    None
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
/// for the network screens and the `status` report, which should show
/// hardware that exists whether or not it's in service. NOT used for "is
/// the network down" fault detection -- see `configured_nics` for that,
/// and why the distinction matters. (The NIC LEDs find their ports from
/// the LED class devices instead -- see `led::nic_led_ports`.)
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
pub fn display_temp(celsius: f32, units: TempUnits) -> (f32, &'static str) {
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

/// Reads a millidegree sysfs value as degrees. Non-finite text ("nan",
/// "inf", which `f32::parse` happily accepts) is never turned into a
/// temperature: it reads as `None`, like any other unreadable value.
pub fn read_sysfs_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli_c| milli_c / 1000.0)
        .filter(|c| c.is_finite())
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

/// Readings below this are not a temperature a real, wired-up sensor
/// gives: they are what an unconnected input reads (this board's `it8625`
/// `temp1`-`temp3` sit at a constant -128C, see `asustor-platform-driver`'s
/// CLAUDE.md). Excluded as "not connected".
pub const PLAUSIBLE_MIN_C: f32 = -20.0;
/// Readings above this are over-range: a sensor that really is that hot,
/// or one that has failed high. Either way the safe response is cooling,
/// so they are *kept* (and fans treat them as critical, see
/// `is_over_range`), never dropped.
pub const PLAUSIBLE_MAX_C: f32 = 125.0;

/// True for a reading above `PLAUSIBLE_MAX_C`: reported by the sensor, but
/// too hot to be anything but an emergency or a fault. Consumers treat it
/// as critical (fans go to `max_pwm`) whatever the configured thresholds.
pub fn is_over_range(celsius: f32) -> bool {
    celsius > PLAUSIBLE_MAX_C
}

/// Reads one `tempN_input` in degrees C. The exact rule:
/// - `tempN_fault` == "1" (a real kernel signal for an open/disconnected
///   thermal diode): `None`, not connected.
/// - unreadable, or not a finite number ("nan"/"inf"): `None`.
/// - below `PLAUSIBLE_MIN_C` (-20C): `None`, garbage from an unwired input.
/// - above `PLAUSIBLE_MAX_C` (125C): `Some`, kept -- over-range is treated
///   as HOT by fan control and the health monitor, not as a missing
///   sensor, because dropping it would let a failed-high sensor *remove*
///   cooling.
/// - otherwise `Some`.
///
/// The lower bound exists because this board's `it8625` onboard
/// `temp1`-`temp3` have no diode wired to them at all and read a constant
/// -128C forever -- with nothing filtering that out, a naive "read every
/// temp this chip has" sensor selector would let a permanently
/// disconnected input stand in for a real reading. A board whose unwired
/// input reads a constant *high* garbage value would instead pin its fan
/// at full speed with a critical alarm; narrow the selector with `input`
/// or `label` to the sensors that are real.
pub fn read_temp_input(hwmon: &str, input: &str) -> Option<f32> {
    if let Ok(s) = std::fs::read_to_string(format!("{hwmon}/{input}_fault"))
        && s.trim() == "1"
    {
        return None;
    }
    read_sysfs_f32(&format!("{hwmon}/{input}_input")).filter(|&t| t >= PLAUSIBLE_MIN_C)
}

/// All temp readings matching a `SensorSelector` (config.rs) across every
/// hwmon instance of the named chip -- e.g. every populated drive bay for
/// `chip = "drivetemp"`, with no need to enumerate them by name. Chips
/// with no matching (or currently unreadable/disconnected) input simply
/// contribute nothing, same as an empty drive bay having no hwmon instance
/// at all.
///
/// `root` is the hwmon class directory (`/sys/class/hwmon` in production, a
/// fixture in tests).
pub fn resolve_selector_in(root: &Path, sel: &crate::config::SensorSelector) -> Vec<f32> {
    let mut out = Vec::new();
    let Some(hwmons) = glob_hwmon_in(root, &sel.chip) else {
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

/// Reads a plain sysfs number (an RPM, say); non-finite text is `None`.
pub fn read_sysfs_raw_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
}

const HWMON_ROOT: &str = "/sys/class/hwmon";

/// The hwmon instances of the chip named `chip`, as sysfs paths in a
/// stable order (`hwmon2` before `hwmon10`). `None` if the hwmon class
/// can't be listed at all; `Some(vec![])` if nothing matches.
///
/// Matching: an instance whose `name` is exactly `chip` wins, and when any
/// exist only those are returned. Only when none match exactly does it
/// fall back to instances whose name merely *starts with* `chip` (so a
/// config written as "nvme" still finds "nvme", and a family prefix keeps
/// working where no exact chip exists). A prefix never adds to an exact
/// match -- "it87" asking for a pwm chip must not quietly pick up a second
/// "it8625" next to the "it87" it meant. For a fan's `pwm_chip`, name the
/// chip exactly.
pub fn glob_hwmon(chip: &str) -> Option<Vec<String>> {
    glob_hwmon_in(Path::new(HWMON_ROOT), chip)
}

/// `glob_hwmon` against a hwmon tree rooted at `root`.
pub fn glob_hwmon_in(root: &Path, chip: &str) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(root).ok()?;
    if chip.is_empty() {
        return Some(Vec::new());
    }
    let (mut exact, mut prefixed) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(name) = std::fs::read_to_string(path.join("name")) else {
            continue;
        };
        let name = name.trim();
        if name == chip {
            exact.push(path);
        } else if name.starts_with(chip) {
            prefixed.push(path);
        }
    }
    let mut found = if exact.is_empty() { prefixed } else { exact };
    // Shorter file name first, so hwmon9 sorts before hwmon10.
    found.sort_by_key(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
        (name.as_ref().map(String::len), name)
    });
    Some(
        found
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    )
}

/// A scratch `/sys/class/hwmon` stand-in for tests, removed on drop.
#[cfg(test)]
pub(crate) struct HwmonTree(pub std::path::PathBuf);

#[cfg(test)]
impl HwmonTree {
    pub fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("lcm-status-hwmon-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        HwmonTree(dir)
    }

    /// Adds `<root>/<dir>/name` (and each `(file, content)` beside it);
    /// returns the instance's path.
    pub fn chip(&self, dir: &str, name: &str, files: &[(&str, &str)]) -> String {
        let path = self.0.join(dir);
        std::fs::create_dir_all(&path).expect("create hwmon dir");
        std::fs::write(path.join("name"), format!("{name}\n")).expect("write name");
        for (file, content) in files {
            std::fs::write(path.join(file), content).expect("write attr");
        }
        path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
impl Drop for HwmonTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    fn run_abandons_a_command_with_runaway_output() {
        // 3 MiB of zeros: far over `OUTPUT_MAX`, and exits successfully.
        assert_eq!(
            run_with_timeout(
                "head",
                &["-c", "3000000", "/dev/zero"],
                Duration::from_secs(10)
            ),
            None
        );
        // Just under the cap is returned whole.
        let ok = run_with_timeout(
            "head",
            &["-c", "100000", "/dev/zero"],
            Duration::from_secs(10),
        );
        assert_eq!(ok.map(|s| s.len()), Some(100_000));
    }

    #[test]
    fn run_gives_up_on_a_wedged_command_holding_the_pipe_open() {
        // The shell is killed at the deadline but its background child
        // keeps the pipe's write end: a reader thread would stay blocked
        // for 30s; here nothing is waiting on it.
        let start = Instant::now();
        assert_eq!(
            run_with_timeout("sh", &["-c", "sleep 30 & wait"], Duration::from_millis(200)),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn run_gives_up_on_a_command_that_closes_stdout_but_keeps_running() {
        let start = Instant::now();
        assert_eq!(
            run_with_timeout(
                "sh",
                &["-c", "exec >&-; sleep 30"],
                Duration::from_millis(200)
            ),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn reap_drops_exited_children_and_keeps_running_ones() {
        let mut quick = Command::new("true").spawn().unwrap();
        quick.wait().unwrap();
        let slow = Command::new("sleep").arg("30").spawn().unwrap();
        let mut list = vec![quick, slow];
        reap(&mut list);
        assert_eq!(list.len(), 1);
        for mut c in list {
            c.kill().unwrap();
            c.wait().unwrap();
        }
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

    fn sel(chip: &str) -> crate::config::SensorSelector {
        crate::config::SensorSelector {
            chip: chip.into(),
            ..Default::default()
        }
    }

    #[test]
    fn glob_hwmon_exact_name_wins_over_prefix_matches() {
        let t = HwmonTree::new("glob-exact");
        t.chip("hwmon0", "it8625", &[]);
        t.chip("hwmon1", "it8625e", &[]);
        t.chip("hwmon2", "coretemp", &[]);
        let got = glob_hwmon_in(&t.0, "it8625").unwrap();
        assert_eq!(got, vec![t.0.join("hwmon0").to_string_lossy().into_owned()]);
    }

    #[test]
    fn glob_hwmon_falls_back_to_prefix_only_without_an_exact_match() {
        let t = HwmonTree::new("glob-prefix");
        t.chip("hwmon0", "it8625e", &[]);
        t.chip("hwmon1", "coretemp", &[]);
        assert_eq!(glob_hwmon_in(&t.0, "it86").unwrap().len(), 1);
        assert!(glob_hwmon_in(&t.0, "nvme").unwrap().is_empty());
        // An empty name matches nothing rather than everything.
        assert!(glob_hwmon_in(&t.0, "").unwrap().is_empty());
        // Unlistable root is None, not empty.
        assert!(glob_hwmon_in(&t.0.join("nope"), "it8625").is_none());
    }

    #[test]
    fn glob_hwmon_is_sorted_numerically_and_keeps_existing_chip_names() {
        let t = HwmonTree::new("glob-sort");
        for (dir, name) in [
            ("hwmon10", "drivetemp"),
            ("hwmon2", "drivetemp"),
            ("hwmon9", "drivetemp"),
            ("hwmon3", "nvme"),
            ("hwmon4", "coretemp"),
        ] {
            t.chip(dir, name, &[]);
        }
        let dirs = |chip: &str| -> Vec<String> {
            glob_hwmon_in(&t.0, chip)
                .unwrap()
                .iter()
                .map(|p| p.rsplit('/').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(dirs("drivetemp"), ["hwmon2", "hwmon9", "hwmon10"]);
        assert_eq!(dirs("nvme"), ["hwmon3"]);
        assert_eq!(dirs("coretemp"), ["hwmon4"]);
    }

    fn read_one(tag: &str, value: &str, fault: Option<&str>) -> Option<f32> {
        let t = HwmonTree::new(tag);
        let mut extra = Vec::new();
        if let Some(f) = fault {
            extra.push(("temp1_fault", f));
        }
        let hwmon = t.chip("hwmon0", "chip", &extra);
        std::fs::write(format!("{hwmon}/temp1_input"), value).unwrap();
        read_temp_input(&hwmon, "temp1")
    }

    #[test]
    fn plausible_readings_pass_through() {
        assert_eq!(read_one("rt-ok", "45000\n", None), Some(45.0));
        assert_eq!(read_one("rt-edge", "125000", None), Some(125.0));
        assert_eq!(read_one("rt-cold", "-20000", None), Some(-20.0));
    }

    #[test]
    fn the_it8625_constant_garbage_is_not_a_reading() {
        // -128C, per the platform driver's CLAUDE.md.
        assert_eq!(read_one("rt-garbage", "-128000", None), None);
    }

    #[test]
    fn over_range_is_kept_and_flagged_hot_not_dropped() {
        let t = read_one("rt-hot", "130000", None).expect("kept");
        assert!(is_over_range(t));
        assert!(!is_over_range(125.0));
        assert_eq!(read_one("rt-huge", "900000", None), Some(900.0));
    }

    #[test]
    fn fault_flag_means_disconnected_even_when_the_value_is_hot() {
        assert_eq!(read_one("rt-fault", "200000", Some("1\n")), None);
        assert_eq!(read_one("rt-nofault", "200000", Some("0\n")), Some(200.0));
    }

    #[test]
    fn non_finite_text_is_never_a_temperature() {
        for bad in ["nan", "NaN", "inf", "-inf", "infinity", "", "garbage"] {
            assert_eq!(read_one("rt-nan", bad, None), None, "{bad:?}");
        }
        // Finite in text but overflowing f32 -> infinite after parsing.
        assert_eq!(read_one("rt-overflow", "1e999", None), None);
        let t = HwmonTree::new("raw-nan");
        let hwmon = t.chip("hwmon0", "it8625", &[("fan1_input", "nan")]);
        assert_eq!(read_sysfs_raw_f32(&format!("{hwmon}/fan1_input")), None);
        std::fs::write(format!("{hwmon}/fan1_input"), "1800\n").unwrap();
        assert_eq!(
            read_sysfs_raw_f32(&format!("{hwmon}/fan1_input")),
            Some(1800.0)
        );
    }

    #[test]
    fn resolve_selector_reads_every_matching_input_and_skips_unwired_ones() {
        let t = HwmonTree::new("resolve");
        t.chip(
            "hwmon0",
            "it8625",
            &[("temp1_input", "-128000"), ("temp2_input", "44000")],
        );
        t.chip("hwmon1", "drivetemp", &[("temp1_input", "38000")]);
        let mut it = resolve_selector_in(&t.0, &sel("it8625"));
        it.sort_by(f32::total_cmp);
        assert_eq!(it, vec![44.0]);
        assert_eq!(resolve_selector_in(&t.0, &sel("drivetemp")), vec![38.0]);
        assert!(resolve_selector_in(&t.0, &sel("coretemp")).is_empty());
    }
}
