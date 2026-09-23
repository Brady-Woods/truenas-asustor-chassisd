//! Data gathering for each status screen. Every function is a plain,
//! synchronous, best-effort snapshot -- callers decide how often to call
//! these (see the refresh-on-display design), not this module.

use crate::config::{Config, TempUnits};
use std::process::Command;

/// Two 16-char-window-ready lines. Line contents may be longer than 16
/// chars; the display layer handles truncation/scrolling.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub line0: String,
    pub line1: String,
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Network: every *physical* interface with a global-scope IPv4, one
/// screen each. Filters out bridges/veth/docker/incus interfaces via
/// /sys/class/net/{iface}/device -- only real hardware NICs have that
/// symlink, which is a more reliable filter than guessing at name
/// prefixes (br-, veth, docker0, incusbr0, ... is an open-ended list that
/// will never fully keep up with every virtual interface naming scheme).
pub fn network() -> Vec<Screen> {
    let out = run("ip", &["-4", "-o", "addr", "show", "scope", "global"]);
    let Some(out) = out else {
        return vec![Screen {
            line0: "NETWORK".into(),
            line1: "no ip info".into(),
        }];
    };

    let screens: Vec<Screen> = out
        .lines()
        .filter_map(|line| {
            // e.g. "2: eth0    inet 192.168.1.196/24 brd ... scope global ..."
            let mut f = line.split_whitespace();
            let iface = f.nth(1)?.trim_end_matches(':').to_string();
            if !std::path::Path::new(&format!("/sys/class/net/{iface}/device")).exists() {
                return None;
            }
            let ip = line
                .split_whitespace()
                .find(|tok| tok.contains('/') && tok.chars().next().unwrap_or(' ').is_ascii_digit())
                .map(|tok| tok.split('/').next().unwrap_or("").to_string())?;
            Some(Screen { line0: iface, line1: ip })
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
/// `network()`), regardless of whether it currently has an address --
/// used for link-state LED checks, not just the display screen.
pub fn physical_nics() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            std::path::Path::new(&format!("/sys/class/net/{name}/device"))
                .exists()
                .then_some(name)
        })
        .collect()
}

/// Storage + RAID health merged: pool name & health on line 0, capacity on
/// line 1 -- one `zpool list` call instead of two.
pub fn pools() -> Vec<Screen> {
    let out = run(
        "zpool",
        &["list", "-H", "-o", "name,size,alloc,capacity,health"],
    );
    let Some(out) = out else {
        return vec![Screen {
            line0: "STORAGE".into(),
            line1: "zpool unavailable".into(),
        }];
    };

    out.lines()
        .map(|line| {
            let mut f = line.split_whitespace();
            let name = f.next().unwrap_or("?");
            let size = f.next().unwrap_or("?");
            let alloc = f.next().unwrap_or("?");
            let cap = f.next().unwrap_or("?");
            let health = f.next().unwrap_or("?");
            Screen {
                line0: format!("{name}: {health}"),
                line1: format!("{alloc}/{size} {cap}"),
            }
        })
        .collect()
}

/// Raw pool name/health pairs, for LED decisions -- separate from the
/// display-formatted `pools()` screens so a status-LED check doesn't need
/// to parse rendered text back apart.
pub fn pool_healths() -> Vec<(String, String)> {
    run("zpool", &["list", "-H", "-o", "name,health"])
        .map(|out| {
            out.lines()
                .filter_map(|line| {
                    let mut f = line.split_whitespace();
                    Some((f.next()?.to_string(), f.next()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
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

/// Per-bay SMART-derived LED state (SATA bays only -- NVMe drives don't
/// have a bay activity LED on this chassis, so they're naturally excluded
/// since `ata_port_for` returns None for them).
pub fn bay_led_states() -> Vec<(u32, crate::led::BayState)> {
    use crate::led::BayState;
    list_disks()
        .iter()
        .filter_map(|d| {
            let bay = ata_port_for(d)?;
            let dev = format!("/dev/{d}");
            let state = run("smartctl", &["-H", "-n", "standby", &dev])
                .map(|out| {
                    if out.contains("in STANDBY") {
                        BayState::Standby
                    } else if out.contains("FAILED") {
                        BayState::Failed
                    } else {
                        BayState::Normal
                    }
                })
                .unwrap_or(BayState::Normal);
            Some((bay, state))
        })
        .collect()
}

/// HDD: SMART health + temperature together, one screen per drive. SATA
/// bay numbers come from the ATA port in `ID_PATH` (e.g. "...-ata-3..." ->
/// BAY3), which is a real, stable physical identifier -- confirmed to
/// match this board's ata1..4 <-> sata1..4 LED convention, not just
/// whatever order the kernel happened to enumerate sdX in.
pub fn hdd() -> Vec<Screen> {
    let disks = list_disks();

    if disks.is_empty() {
        return vec![Screen {
            line0: "HDD".into(),
            line1: "no disks found".into(),
        }];
    }

    let bay_temps = drivetemps_by_ata_port();

    disks
        .iter()
        .map(|d| {
            let dev = format!("/dev/{d}");
            let status = run("smartctl", &["-H", "-n", "standby", &dev])
                .map(|out| {
                    if out.contains("in STANDBY") {
                        "STANDBY".to_string()
                    } else if out.contains("PASSED") {
                        "PASSED".to_string()
                    } else if out.contains("FAILED") {
                        "FAILED".to_string()
                    } else {
                        "UNKNOWN".to_string()
                    }
                })
                .unwrap_or_else(|| "N/A".to_string());

            let (label, temp) = match ata_port_for(d) {
                Some(bay) => (format!("BAY{bay} {d}"), bay_temps.get(&bay).copied()),
                None => (d.clone(), nvme_temp_for(d)),
            };

            let line1 = match temp {
                Some(t) => format!("{status} {:.0}C", t),
                None => status,
            };
            Screen { line0: label, line1 }
        })
        .collect()
}

/// CPU temperature + the one real fan on this board (fan2/fan3 read a
/// constant 0 RPM with ALARM -- confirmed unpopulated headers, not failed
/// fans, so they're deliberately excluded rather than shown as an alert).
pub fn cpu_and_fan(cfg: &Config) -> Vec<Screen> {
    let mut screens = Vec::new();
    if let Some(cpu_c) = coretemp_package() {
        screens.push(temp_screen("CPU", cpu_c, cfg));
    }
    if let Some(rpm) = fan1_rpm() {
        screens.push(Screen {
            line0: "FAN".into(),
            line1: format!("{rpm:.0} RPM"),
        });
    }
    if screens.is_empty() {
        screens.push(Screen {
            line0: "TEMPERATURE".into(),
            line1: "no sensors found".into(),
        });
    }
    screens
}

fn temp_screen(label: &str, celsius: f32, cfg: &Config) -> Screen {
    let (val, unit) = match cfg.temperature.units {
        TempUnits::C => (celsius, "C"),
        TempUnits::F => (celsius * 9.0 / 5.0 + 32.0, "F"),
    };
    let warn = celsius >= cfg.temperature.warn_threshold;
    Screen {
        line0: label.to_string(),
        line1: format!("{:.0}{unit}{}", val, if warn { " !" } else { "" }),
    }
}

/// Resolves a block device to its physical bay number via
/// /sys/class/ata_port/ataX/port_no -- the same stable, per-controller
/// source the LED driver's own bay triggers use. That driver computes the
/// LED name from `ap->port_no + 1` in kernel source (0-indexed internal
/// field), but the "+1" already happened before the value reached sysfs --
/// confirmed empirically: ata1's port_no reads back as 1, not 0. So the
/// sysfs value is already the human-facing bay number as-is.
/// Deliberately NOT the "ataN" name itself: that's a global counter across
/// every SATA controller in probe order, so it only happens to match bay
/// number today because the ASM1164 is the system's only controller. It
/// would silently shift if a second controller (e.g. a PCIe SATA card)
/// were ever added.
fn ata_port_for(dev_name: &str) -> Option<u32> {
    let out = run("udevadm", &["info", "-q", "property", &format!("/dev/{dev_name}")])?;
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
    let controller = dev_name.rsplit_once('n').map(|(ctrl, _ns)| ctrl).unwrap_or(dev_name);
    for hwmon in glob_hwmon("nvme")? {
        let device_link = std::fs::read_link(format!("{hwmon}/device")).ok()?;
        let link_name = device_link.file_name()?.to_string_lossy().to_string();
        if link_name == controller {
            return read_sysfs_f32(&format!("{hwmon}/temp1_input"));
        }
    }
    None
}

pub(crate) fn read_sysfs_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli_c| milli_c / 1000.0)
}

/// Every `tempN` input a hwmon instance exposes (base names only, e.g.
/// `["temp1", "temp2"]` -- callers append `_input`/`_label`/`_fault`
/// themselves). Used to enumerate a chip's sensors without hardcoding how
/// many it has.
pub(crate) fn temp_inputs(hwmon: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(hwmon) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix("_input")
                .filter(|n| n.starts_with("temp"))
                .map(|n| n.to_string())
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
/// curve's max() up to full speed forever.
pub(crate) fn read_temp_input(hwmon: &str, input: &str) -> Option<f32> {
    if let Ok(s) = std::fs::read_to_string(format!("{hwmon}/{input}_fault")) {
        if s.trim() == "1" {
            return None;
        }
    }
    const PLAUSIBLE_MIN_C: f32 = -20.0;
    const PLAUSIBLE_MAX_C: f32 = 125.0;
    read_sysfs_f32(&format!("{hwmon}/{input}_input"))
        .filter(|&t| (PLAUSIBLE_MIN_C..=PLAUSIBLE_MAX_C).contains(&t))
}

/// All temp readings matching a `SensorSelector` (config.rs) across every
/// hwmon instance of the named chip -- e.g. every populated drive bay for
/// `chip = "drivetemp"`, with no need to enumerate them by name. Chips
/// with no matching (or currently unreadable/disconnected) input simply
/// contribute nothing, same as an empty drive bay having no hwmon instance
/// at all.
pub(crate) fn resolve_selector(sel: &crate::config::SensorSelector) -> Vec<f32> {
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
                let label = std::fs::read_to_string(format!("{hwmon}/{input}_label")).unwrap_or_default();
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
pub(crate) fn all_hwmon() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path().to_string_lossy().to_string();
            let name = std::fs::read_to_string(format!("{path}/name")).ok()?.trim().to_string();
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
pub(crate) fn all_connected_temps() -> Vec<(String, f32)> {
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
            let desc = match label {
                Some(l) => format!("{chip} {input} \"{l}\""),
                None => format!("{chip} {input}"),
            };
            out.push((desc, t));
        }
    }
    out
}

pub(crate) fn coretemp_package() -> Option<f32> {
    for hwmon in glob_hwmon("coretemp")? {
        for entry in std::fs::read_dir(&hwmon).ok()?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("temp") && name.ends_with("_label") {
                if let Ok(label) = std::fs::read_to_string(entry.path()) {
                    if label.trim().starts_with("Package") {
                        let input = entry.path().to_string_lossy().replace("_label", "_input");
                        return read_sysfs_f32(&input);
                    }
                }
            }
        }
    }
    None
}

/// Maps real bay number (via `ata_port_for`, not hwmon enumeration order)
/// to that bay's drive temperature.
fn drivetemps_by_ata_port() -> std::collections::HashMap<u32, f32> {
    let mut out = std::collections::HashMap::new();
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
        if let Some(rpm) = read_sysfs_raw_f32(&format!("{hwmon}/fan1_input")) {
            if rpm > 0.0 {
                return Some(rpm);
            }
        }
    }
    None
}

pub(crate) fn read_sysfs_raw_f32(path: &str) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(crate) fn glob_hwmon(name_prefix: &str) -> Option<Vec<String>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        if let Ok(name) = std::fs::read_to_string(path.join("name")) {
            if name.trim().starts_with(name_prefix) {
                found.push(path.to_string_lossy().to_string());
            }
        }
    }
    Some(found)
}

/// Docker: any container not "healthy" or plain "Up" (running, no healthcheck).
/// Returns one screen per problem container; empty if everything's fine.
pub fn docker_issues(ignore: &[String]) -> Vec<Screen> {
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
            let ok = status.contains("(healthy)") || (status.starts_with("Up") && !status.contains('('));
            if ok {
                None
            } else {
                Some(Screen {
                    line0: format!("DOCKER {name}"),
                    line1: status.to_string(),
                })
            }
        })
        .collect()
}
