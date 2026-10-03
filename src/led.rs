//! Front-panel LED control, thin sysfs wrapper + the status-LED decision
//! logic. Patterns are exactly what's documented for this hardware's LED
//! class devices under /sys/class/leds -- see the asustord project's
//! LED-MODES.md for the reference this was built against.
//!
//! Also here: front LED brightness (the IT8625E's `pwm3`, see
//! `FRONT_BRIGHTNESS_CHIP`) and the bay LEDs' idle style (the platform
//! driver's `disk_led_ready` parameter, see `set_bay_mode`).

use crate::socket::Level;
use serde::Deserialize;
use std::path::Path;

#[cfg_attr(test, allow(dead_code))]
const LEDS: &str = "/sys/class/leds";

/// Best-effort: most callers can't do anything useful about a failed LED
/// write. Where a failure does matter (the NIC LEDs' `netdev` setup), the
/// caller reads the result back with `read_attr` instead -- see
/// `apply_nic_port` for why the write's own error isn't the right signal
/// there.
fn write_attr(led: &str, attr: &str, value: &str) {
    #[cfg(not(test))]
    let _ = std::fs::write(format!("{LEDS}/{led}/{attr}"), value);
    #[cfg(test)]
    test_writes::record(led, attr, value);
}

/// Trimmed contents of an LED attribute, `None` if it doesn't exist (e.g.
/// a trigger-specific attribute while a different trigger is selected).
fn read_attr(led: &str, attr: &str) -> Option<String> {
    #[cfg(not(test))]
    return std::fs::read_to_string(format!("{LEDS}/{led}/{attr}"))
        .ok()
        .map(|s| s.trim().to_string());
    #[cfg(test)]
    return test_writes::value(led, attr);
}

/// Under `cfg(test)` LED writes never reach sysfs; they're recorded per
/// test thread instead, so state-machine tests can assert on them (and
/// can't flip a real front panel if run on the NAS itself). Reads see the
/// last value written (or `preset`), so read-back logic is testable too.
#[cfg(test)]
pub mod test_writes {
    use std::cell::RefCell;
    use std::collections::HashMap;

    type Key = (String, String);

    thread_local! {
        static WRITES: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
        static VALUES: RefCell<Option<HashMap<Key, String>>> = const { RefCell::new(None) };
        static REJECTED: RefCell<Vec<Key>> = const { RefCell::new(Vec::new()) };
    }

    /// Makes writes to `led`'s `attr` fail: still recorded, but a later
    /// read doesn't see them (like writing a trigger that isn't loaded).
    pub fn reject(led: &str, attr: &str) {
        REJECTED.with(|r| r.borrow_mut().push((led.to_string(), attr.to_string())));
    }

    fn store(led: &str, attr: &str, value: &str) {
        let key = (led.to_string(), attr.to_string());
        if REJECTED.with(|r| r.borrow().contains(&key)) {
            return;
        }
        // sysfs shows the selected trigger in brackets on read.
        let shown = if attr == "trigger" {
            format!("[{value}]")
        } else {
            value.to_string()
        };
        VALUES.with(|v| {
            v.borrow_mut()
                .get_or_insert_with(HashMap::new)
                .insert((led.to_string(), attr.to_string()), shown);
        });
    }

    pub fn record(led: &str, attr: &str, value: &str) {
        WRITES.with(|w| {
            w.borrow_mut()
                .push((led.to_string(), attr.to_string(), value.to_string()));
        });
        store(led, attr, value);
    }

    /// Sets what a read of `led`'s `attr` returns, without recording a
    /// write.
    pub fn preset(led: &str, attr: &str, value: &str) {
        store(led, attr, value);
    }

    pub fn value(led: &str, attr: &str) -> Option<String> {
        VALUES.with(|v| {
            v.borrow()
                .as_ref()?
                .get(&(led.to_string(), attr.to_string()))
                .cloned()
        })
    }

    /// Drains and returns every write recorded on this thread so far.
    pub fn take() -> Vec<(String, String, String)> {
        WRITES.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }

    /// True if `led`'s `attr` was written as `value` since the last `take`.
    pub fn wrote(led: &str, attr: &str, value: &str) -> bool {
        WRITES.with(|w| {
            w.borrow()
                .iter()
                .any(|(l, a, v)| l == led && a == attr && v == value)
        })
    }
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

/// How fast a `LOCATE` blinks, both on and off, everywhere it blinks
/// anything: bay green+red, status green+red, and the blue power LED.
///
/// Locate has to be unmistakable at a glance against every other front-panel
/// state, so it differs from all of them on two axes at once:
///
/// - **Colour.** It's the only bay state that ever lights green *and* red
///   together (amber) -- `Failed`/`Alert` are red-only, `Standby` and
///   `Normal` are green-only. On the status LED the only other amber is the
///   *solid* `Warning`.
/// - **Rate.** 250/250 is 2x the factory `Degraded` 500/500, 4x the
///   `Alert`/`CriticalFlashing` 1000/1000, and a 50% duty cycle unlike
///   `Standby`'s 250/9750 blip. It stays clear of the 125/125 that was
///   found too fast to read as blinking at all (see `CriticalFlashing`).
///
/// Strict green/red *alternation* would be prettier but can't be done
/// reliably: the red LEDs are blinked by `ledtrig-timer` (a software timer
/// per LED, restarted from scratch by every `delay_on`/`delay_off` write),
/// and nothing lets userspace set one timer's phase relative to another's.
/// Writing both back to back instead starts them within the same jiffy, so
/// they flash together -- and if one of them did turn out to be blinked by
/// hardware with its own clock, the result is an irregular green/red/amber
/// flicker, which is still unlike any other state.
pub const LOCATE_BLINK_MS: u32 = 250;

/// Every status-LED pattern: the five documented health patterns, worst
/// last, plus two that aren't health verdicts at all (`Locate`, `Off`) --
/// `state::recompute_status_led` picks between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPattern {
    /// Solid green.
    Ok,
    /// Solid amber (green+red both on) -- "our addition" per the doc, not a
    /// factory pattern. Originally just "network down"; now the generic
    /// non-critical warning indicator (some monitored NICs down, a temp/fan
    /// warning, etc.) -- see `state::recompute_status_led`. Deliberately
    /// coarse: which specific thing tripped it is on the LCD/syslog, not
    /// encoded in the LED color.
    Warning,
    /// Green solid, red flashing 500/500 -- factory RAID-degraded pattern.
    Degraded,
    /// Solid red -- factory "malfunction".
    Failed,
    /// Red flashing 1000/1000, green off -- a `critical` condition.
    CriticalFlashing,
    /// Green and red flashing together at `LOCATE_BLINK_MS` (amber flash)
    /// -- a chassis `LOCATE` is active.
    Locate,
    /// Both dark -- night mode.
    Off,
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
        StatusPattern::Locate => {
            set_blink("green:status", LOCATE_BLINK_MS, LOCATE_BLINK_MS);
            set_blink("red:status", LOCATE_BLINK_MS, LOCATE_BLINK_MS);
        }
        StatusPattern::Off => {
            set_solid("green:status", false);
            set_solid("red:status", false);
        }
    }
}

/// The front "Power" LED (bi-color blue/red, GPIO-driven -- per
/// mafredri/asustor-platform-driver's CLAUDE.md), a separate device from
/// the status LED. Only night mode and a chassis `LOCATE` ever change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerPattern {
    /// Solid blue: awake.
    On,
    /// Dark: night mode.
    Off,
    /// Blue flashing at `LOCATE_BLINK_MS` -- nothing else ever blinks it.
    Locate,
}

pub fn set_power(pattern: PowerPattern) {
    match pattern {
        PowerPattern::On => set_solid("blue:power", true),
        PowerPattern::Off => set_solid("blue:power", false),
        PowerPattern::Locate => set_blink("blue:power", LOCATE_BLINK_MS, LOCATE_BLINK_MS),
    }
    set_solid("red:power", false);
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
    /// A `LOCATE bay=N` is active: green and red flashing together at
    /// `LOCATE_BLINK_MS` -- see that constant for why this pattern. Never
    /// a health verdict (`hal::bay_led_states` doesn't produce it); it's
    /// layered on top of one by `state::AppState::bay_led_state`.
    Locate,
}

/// Bay LEDs: red on for a failed drive; green slow-flash for standby/spun
/// down; otherwise hand activity back to the driver's own automatic
/// trigger rather than leaving our last override in place forever.
pub fn set_bay(bay: u32, state: BayState) {
    let green = format!("sata{bay}:green:disk");
    match state {
        BayState::Normal => write_attr(&green, "trigger", &format!("asustor-sata{bay}")),
        BayState::Failed | BayState::Alert => set_solid(&green, false),
        BayState::Standby => set_blink(&green, 250, 9750),
        BayState::Locate => set_blink(&green, LOCATE_BLINK_MS, LOCATE_BLINK_MS),
    }
    set_bay_red(bay, state);
}

/// `set_bay` as night mode leaves a bay: green dark, red still showing
/// `Failed`/`Alert` (night mode never darkens red bay LEDs, see
/// `enter_night_mode`). `Locate` is the exception and lights in full even
/// at night -- finding a drive is the whole point of it.
pub fn set_bay_night(bay: u32, state: BayState) {
    if state == BayState::Locate {
        set_bay(bay, state);
        return;
    }
    set_solid(&format!("sata{bay}:green:disk"), false);
    set_bay_red(bay, state);
}

fn set_bay_red(bay: u32, state: BayState) {
    let red = format!("sata{bay}:red:disk");
    match state {
        BayState::Normal | BayState::Standby => set_solid(&red, false),
        BayState::Failed => set_solid(&red, true),
        BayState::Alert => set_blink(&red, 1000, 1000),
        BayState::Locate => set_blink(&red, LOCATE_BLINK_MS, LOCATE_BLINK_MS),
    }
}

/// Night mode: bay green LEDs off, and -- when `front` is set -- the
/// status/power/network/USB LEDs too. `front` is false when
/// `[led] night_brightness` dims those instead of darkening them (the bay
/// LEDs aren't on the brightness PWM, so they always go dark). Red bay
/// LEDs are deliberately left alone so a real failure still shows even
/// while "asleep", same principle as a critical alert waking the LCD.
/// Returns the NIC LED result (see `set_nic_mode`), the one part here
/// whose failure is worth surfacing.
pub fn enter_night_mode(front: bool) -> Result<(), String> {
    for bay in 1..=4 {
        write_attr(&format!("sata{bay}:green:disk"), "trigger", "none");
        set_solid(&format!("sata{bay}:green:disk"), false);
        // sata{bay}:red:disk intentionally untouched.
    }
    if !front {
        return Ok(());
    }

    set_status(StatusPattern::Off);
    set_power(PowerPattern::Off);

    set_solid("green:usb", false);
    write_attr("green:usb", "trigger", "none");

    // `blue:lan` isn't itself an LED -- it's the shared power rail for
    // both front LAN LEDs (per the same driver notes); off darkens both
    // regardless of the per-port PHY config, which is switched off below
    // as well so the ports stay dark even if something re-enables the rail.
    set_solid("blue:lan", false);

    apply_nic_leds(FrontLanState::Off)
}

/// Restores factory-automatic behavior for everything `enter_night_mode`
/// (with the same `front`) touched, and the *configured* `nic_mode` on the
/// NIC LEDs. (This used to restore "link" unconditionally and leave the
/// caller to then apply `activity` if configured -- two PHY
/// reconfigurations, and a visible blip, per wake.)
pub fn exit_night_mode(nic_mode: NicLedMode, front: bool) -> Result<(), String> {
    for bay in 1..=4 {
        write_attr(
            &format!("sata{bay}:green:disk"),
            "trigger",
            &format!("asustor-sata{bay}"),
        );
    }
    if !front {
        return Ok(());
    }

    write_attr("green:status", "trigger", "none");
    set_solid("green:status", true);
    write_attr("red:status", "trigger", "panic");
    set_solid("red:status", false);

    set_power(PowerPattern::On);

    write_attr("green:usb", "trigger", "asustor-front-usb");

    set_solid("blue:lan", true);

    set_nic_mode(nic_mode)
}

/// NIC LED mode, applied from config at startup and on every wake from
/// night mode (not health-driven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicLedMode {
    Link,
    Activity,
}

/// What a front LAN LED should show: a configured `NicLedMode`, or night
/// mode's `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontLanState {
    Link,
    Activity,
    Off,
}

impl From<NicLedMode> for FrontLanState {
    fn from(mode: NicLedMode) -> Self {
        match mode {
            NicLedMode::Link => FrontLanState::Link,
            NicLedMode::Activity => FrontLanState::Activity,
        }
    }
}

/// The `netdev` trigger attributes managed here, in the order they're
/// written: `rx` immediately before `tx`, both last. r8169 can only
/// offload rx and tx *together* (`r8169_trigger_mode_is_valid`), so the
/// instant between those two writes is an unsupported mode: the kernel
/// rejects the first write with EOPNOTSUPP but still stores the bit (so
/// the second write completes a valid mode), and r8169 switches the LED
/// off for that instant. No ordering avoids that one rejected write when
/// activity is switched on or off; what the order does guarantee is that
/// nothing else is written while rx != tx, which would be rejected (and
/// blank the LED) the same way.
const NETDEV_ATTRS: [&str; 6] = ["link_10", "link_100", "link_1000", "link_2500", "rx", "tx"];

/// Wanted `NETDEV_ATTRS` values for the two PHY LED channels wired to the
/// front LED (`-0` and `-1`, OR'd onto the same blue LED -- see the
/// platform driver's CLAUDE.md). Link is the factory config: `-0` on a
/// 2.5G link, `-1` on 10/100/1000. Activity is `-0` traffic-only (dark at
/// idle, a visible flicker on traffic -- link+activity just looks solid,
/// since the RTL8125 blinks *off* on traffic) with `-1` off. `-2`/`-3`
/// (presumably the rear RJ45's LEDs) are never touched.
fn front_lan_plan(state: FrontLanState) -> [(&'static str, [bool; 6]); 2] {
    const OFF: [bool; 6] = [false; 6];
    match state {
        FrontLanState::Link => [
            ("0", [false, false, false, true, false, false]),
            ("1", [true, true, true, false, false, false]),
        ],
        FrontLanState::Activity => [("0", [false, false, false, false, true, true]), ("1", OFF)],
        FrontLanState::Off => [("0", OFF), ("1", OFF)],
    }
}

/// Every NIC with r8169 PHY LEDs (`{iface}-0::lan`), sorted -- on this
/// board both RTL8125s, `enp2s0` and `enp3s0`. Found from the LED class
/// devices rather than `hal::physical_nics()`, which also lists NICs with
/// no such LEDs (this board's AQC113, `enp9s0`), where every write could
/// only fail. r8169 builds these names from the PCI address
/// (`r8169_get_led_name`), the same way the interface's default
/// predictable name is built.
fn nic_led_ports() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(LEDS) else {
        return Vec::new();
    };
    let mut ports: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix("-0::lan")
                .map(str::to_string)
        })
        .collect();
    ports.sort();
    ports
}

/// Applies `mode` to every NIC's front LED. `Err` describes what didn't
/// take (one entry per failing port) for the caller to log -- once, not on
/// every call: see `AppState::note_nic_leds`.
pub fn set_nic_mode(mode: NicLedMode) -> Result<(), String> {
    apply_nic_leds(mode.into())
}

fn apply_nic_leds(state: FrontLanState) -> Result<(), String> {
    let problems: Vec<String> = nic_led_ports()
        .iter()
        .filter_map(|port| apply_nic_port(port, state).err())
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Puts one NIC's front LED channels into `state` through the
/// hardware-offloaded `netdev` trigger, then reads everything back to
/// confirm it took.
///
/// Checked by read-back rather than by each write's result: the sequence
/// legitimately includes one rejected write per rx/tx change (see
/// `NETDEV_ATTRS`), and a silently ignored *real* failure is exactly the
/// bug this replaces -- with `ledtrig-netdev` not loaded, selecting
/// `netdev` fails, none of its attributes exist, and every write had been
/// failing without a trace. `offloaded` (1 = the PHY itself is driving
/// the LED in the stored mode) is the final word.
///
/// Only attributes that differ from the wanted value are written, so
/// re-applying the current mode doesn't blip the LED. Selecting `netdev`
/// reads the PHY's current config into the trigger (non-destructive), so
/// that step alone never changes what the LED shows.
fn apply_nic_port(port: &str, state: FrontLanState) -> Result<(), String> {
    let as_value = |on: bool| if on { "1" } else { "0" };
    for (channel, want) in front_lan_plan(state) {
        let led = format!("{port}-{channel}::lan");
        if !trigger_selected(&led, "netdev") {
            write_attr(&led, "trigger", "netdev");
            if !trigger_selected(&led, "netdev") {
                return Err(format!(
                    "{led}: couldn't select the netdev trigger (ledtrig-netdev not loaded?)"
                ));
            }
        }
        for (attr, &on) in NETDEV_ATTRS.iter().zip(&want) {
            if read_attr(&led, attr).as_deref() != Some(as_value(on)) {
                write_attr(&led, attr, as_value(on));
            }
        }
        for (attr, &on) in NETDEV_ATTRS.iter().zip(&want) {
            let got = read_attr(&led, attr);
            if got.as_deref() != Some(as_value(on)) {
                return Err(format!(
                    "{led}: {attr} reads {}, wanted {}",
                    got.as_deref().unwrap_or("nothing"),
                    as_value(on)
                ));
            }
        }
        let offloaded = read_attr(&led, "offloaded");
        if offloaded.as_deref() != Some("1") {
            return Err(format!(
                "{led}: not hardware-offloaded (offloaded={}), so not showing the configured mode",
                offloaded.as_deref().unwrap_or("missing")
            ));
        }
    }
    Ok(())
}

/// Whether `led`'s current trigger (the one shown in brackets in its
/// `trigger` file) is `name`.
fn trigger_selected(led: &str, name: &str) -> bool {
    let selected = format!("[{name}]");
    read_attr(led, "trigger").is_some_and(|t| t.split_whitespace().any(|w| w == selected))
}

/// hwmon chip whose `FRONT_BRIGHTNESS_PWM` output sets the front LEDs'
/// brightness. Found from ADM's `Hal_Led_Set_Brightness` (libnhal), which
/// on this platform writes `255 - level` to the IT8625E's PWM3 duty
/// register, and confirmed live (2026-10-02): sweeping `pwm3` smoothly
/// dims the power, status, LAN and USB LEDs -- not the bay LEDs, which
/// stay at fixed brightness (`pwm2`, the other output at the same odd
/// BIOS default, was swept too and does nothing visible).
pub const FRONT_BRIGHTNESS_CHIP: &str = "it8625";
/// See `FRONT_BRIGHTNESS_CHIP`. Inverted: 0 = brightest, 255 = off.
pub const FRONT_BRIGHTNESS_PWM: u32 = 3;

/// `percent` (0-100, clamped) as the inverted `pwm3` duty. 0% is 255,
/// which it87 treats as "full speed" and flips `pwm3_enable` to 0 for --
/// still full duty, so still off; `set_front_brightness` puts manual mode
/// back before every later write.
pub fn brightness_pwm(percent: u8) -> u8 {
    let lit = u16::from(percent.min(100)) * 255 / 100;
    255 - u8::try_from(lit).unwrap_or(255)
}

#[cfg_attr(test, allow(dead_code))]
fn front_brightness_hwmon() -> Option<String> {
    crate::hal::glob_hwmon(FRONT_BRIGHTNESS_CHIP)?
        .into_iter()
        .next()
}

/// The front LEDs' current brightness as a raw (inverted) duty, so a
/// day level can be put back after night mode dims it when only
/// `[led] night_brightness` is configured.
pub fn read_front_brightness() -> Option<u8> {
    #[cfg(not(test))]
    return std::fs::read_to_string(format!(
        "{}/pwm{FRONT_BRIGHTNESS_PWM}",
        front_brightness_hwmon()?
    ))
    .ok()?
    .trim()
    .parse()
    .ok();
    #[cfg(test)]
    return test_writes::value("front-brightness", "pwm")
        .and_then(|v| v.parse().ok())
        .or(Some(51));
}

/// Sets the front LEDs' brightness to raw (inverted) duty `pwm`. Manual
/// mode (`pwm3_enable` = 1) goes first every time: the BIOS leaves it
/// manual, but a 255 write (0%) flips it to 0, and in automatic mode the
/// duty would follow the chip's own (unconnected) temp inputs instead.
#[cfg_attr(test, allow(clippy::unnecessary_wraps))]
pub fn set_front_brightness(pwm: u8) -> Result<(), String> {
    #[cfg(not(test))]
    {
        let hwmon = front_brightness_hwmon()
            .ok_or_else(|| format!("no {FRONT_BRIGHTNESS_CHIP} hwmon found"))?;
        let base = format!("{hwmon}/pwm{FRONT_BRIGHTNESS_PWM}");
        std::fs::write(format!("{base}_enable"), "1")
            .and_then(|()| std::fs::write(&base, pwm.to_string()))
            .map_err(|e| format!("{base}: {e}"))
    }
    #[cfg(test)]
    {
        test_writes::record("front-brightness", "pwm", &pwm.to_string());
        Ok(())
    }
}

/// How the bay green LEDs show a present disk, via the platform driver's
/// `disk_led_ready` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BayLedMode {
    /// Solid while a disk is present, blinks off on access (factory).
    Ready,
    /// Dark at idle, flashes on access.
    Activity,
}

#[cfg_attr(test, allow(dead_code))]
const DISK_LED_READY: &str = "/sys/module/asustor/parameters/disk_led_ready";

/// Applies `mode` through `disk_led_ready`, writing only if it differs
/// (a no-op write would still restart any blink in flight). The parameter
/// is only writable at runtime with the platform driver's
/// "Make `disk_led_ready` writable at runtime" change; on an older driver
/// the `Err` says so, since the only other way to change it is reloading
/// `asustor`, which power-cycles the LCD -- not something to do from here.
#[cfg_attr(test, allow(clippy::unnecessary_wraps))]
pub fn set_bay_mode(mode: BayLedMode) -> Result<(), String> {
    let want = match mode {
        BayLedMode::Ready => "Y",
        BayLedMode::Activity => "N",
    };
    #[cfg(not(test))]
    {
        let current = std::fs::read_to_string(DISK_LED_READY)
            .map_err(|e| format!("{DISK_LED_READY}: {e} (asustor driver not loaded?)"))?;
        if current.trim() == want {
            return Ok(());
        }
        std::fs::write(DISK_LED_READY, if want == "Y" { "1" } else { "0" }).map_err(|e| {
            format!(
                "{DISK_LED_READY}: {e} -- this asustor driver only reads it at load; \
                 it needs the runtime-writable disk_led_ready change"
            )
        })
    }
    #[cfg(test)]
    {
        test_writes::record("disk_led_ready", "value", want);
        Ok(())
    }
}

/// LED triggers selected here that live in modules TrueNAS doesn't load
/// by default: `timer` (every blink pattern -- RAID degraded, critical
/// alert, bay standby) and `netdev` (`nic_mode` and night mode on the NIC
/// LEDs). Both ship with the stock TrueNAS kernel.
const TRIGGER_MODULES: [(&str, &str); 2] =
    [("timer", "ledtrig-timer"), ("netdev", "ledtrig-netdev")];

/// Loads each of `TRIGGER_MODULES` whose trigger isn't registered yet, and
/// logs a syslog WARNING for any still missing afterward -- without it,
/// the LEDs using that trigger just keep whatever state they had. Called
/// once at daemon startup (which runs as root). Not persisted to
/// `/etc/modules-load.d`: `/etc` doesn't survive a TrueNAS update, and
/// this runs on every start anyway.
pub fn ensure_trigger_modules() {
    for (trigger, module) in TRIGGER_MODULES {
        if trigger_registered(trigger) {
            continue;
        }
        let loaded = std::process::Command::new("modprobe")
            .arg(module)
            .status()
            .is_ok_and(|s| s.success());
        if !loaded || !trigger_registered(trigger) {
            crate::syslog::warning(&format!(
                "LED trigger '{trigger}' unavailable (`modprobe {module}` failed); \
                 LEDs that use it won't change"
            ));
        }
    }
}

/// Whether trigger `name` is registered. Every LED's `trigger` file lists
/// every registered (non-private) trigger, so any LED will do.
fn trigger_registered(name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(LEDS) else {
        return false;
    };
    entries.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("trigger")).is_ok_and(|t| {
            t.split_whitespace()
                .any(|w| w.trim_start_matches('[').trim_end_matches(']') == name)
        })
    })
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

#[cfg(test)]
mod tests {
    #[test]
    fn brightness_percent_maps_to_inverted_pwm() {
        assert_eq!(brightness_pwm(100), 0);
        assert_eq!(brightness_pwm(0), 255);
        assert_eq!(brightness_pwm(30), 179); // ADM's default
        assert_eq!(brightness_pwm(80), 51); // about where the BIOS leaves it
        assert_eq!(brightness_pwm(200), 0);
    }

    use super::*;

    /// Presets `port`'s two front channels as the kernel would show them
    /// with `netdev` already selected and offloaded in `state`.
    fn preset_port(port: &str, state: FrontLanState) {
        for (channel, want) in front_lan_plan(state) {
            let led = format!("{port}-{channel}::lan");
            test_writes::preset(&led, "trigger", "netdev");
            test_writes::preset(&led, "offloaded", "1");
            for (attr, &on) in NETDEV_ATTRS.iter().zip(&want) {
                test_writes::preset(&led, attr, if on { "1" } else { "0" });
            }
        }
    }

    /// (attr, value) written to `led`, in order, out of `writes`.
    fn written_to(writes: &[(String, String, String)], led: &str) -> Vec<(String, String)> {
        writes
            .iter()
            .filter(|(l, _, _)| l == led)
            .map(|(_, a, v)| (a.clone(), v.clone()))
            .collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    #[test]
    fn link_mode_selects_netdev_first_then_writes_the_factory_config() {
        // Trigger "none", as found live: netdev's attributes don't exist yet.
        for led in ["enp2s0-0::lan", "enp2s0-1::lan"] {
            test_writes::preset(led, "trigger", "none");
            test_writes::preset(led, "offloaded", "1");
        }
        test_writes::take();
        assert_eq!(apply_nic_port("enp2s0", FrontLanState::Link), Ok(()));
        let writes = test_writes::take();
        assert_eq!(
            written_to(&writes, "enp2s0-0::lan")[0],
            pairs(&[("trigger", "netdev")])[0]
        );
        assert_eq!(
            test_writes::value("enp2s0-0::lan", "link_2500").as_deref(),
            Some("1")
        );
        for attr in ["link_10", "link_100", "link_1000"] {
            assert_eq!(
                test_writes::value("enp2s0-1::lan", attr).as_deref(),
                Some("1")
            );
        }
    }

    #[test]
    fn link_to_activity_writes_rx_then_tx_with_nothing_in_between() {
        preset_port("enp2s0", FrontLanState::Link);
        test_writes::take();
        assert_eq!(apply_nic_port("enp2s0", FrontLanState::Activity), Ok(()));
        let writes = test_writes::take();
        assert_eq!(
            written_to(&writes, "enp2s0-0::lan"),
            pairs(&[("link_2500", "0"), ("rx", "1"), ("tx", "1")])
        );
        assert_eq!(
            written_to(&writes, "enp2s0-1::lan"),
            pairs(&[("link_10", "0"), ("link_100", "0"), ("link_1000", "0")])
        );
    }

    #[test]
    fn activity_to_night_off_clears_rx_then_tx() {
        preset_port("enp3s0", FrontLanState::Activity);
        test_writes::take();
        assert_eq!(apply_nic_port("enp3s0", FrontLanState::Off), Ok(()));
        assert_eq!(
            written_to(&test_writes::take(), "enp3s0-0::lan"),
            pairs(&[("rx", "0"), ("tx", "0")])
        );
    }

    #[test]
    fn reapplying_the_current_mode_writes_nothing() {
        preset_port("enp2s0", FrontLanState::Activity);
        test_writes::take();
        assert_eq!(apply_nic_port("enp2s0", FrontLanState::Activity), Ok(()));
        assert_eq!(test_writes::take(), Vec::new());
    }

    #[test]
    fn missing_netdev_trigger_is_reported_not_swallowed() {
        test_writes::preset("enp2s0-0::lan", "trigger", "none");
        test_writes::reject("enp2s0-0::lan", "trigger");
        let err = apply_nic_port("enp2s0", FrontLanState::Link).unwrap_err();
        assert!(err.contains("ledtrig-netdev"), "{err}");
    }

    #[test]
    fn mode_not_offloaded_is_reported() {
        preset_port("enp2s0", FrontLanState::Link);
        test_writes::preset("enp2s0-0::lan", "offloaded", "0");
        let err = apply_nic_port("enp2s0", FrontLanState::Link).unwrap_err();
        assert!(err.contains("offloaded=0"), "{err}");
    }

    #[test]
    fn every_front_plan_ends_with_rx_and_tx_equal() {
        // r8169 can't offload rx without tx or vice versa; a final state
        // with them unequal would leave the LED switched off.
        for state in [
            FrontLanState::Link,
            FrontLanState::Activity,
            FrontLanState::Off,
        ] {
            for (_, want) in front_lan_plan(state) {
                assert_eq!(want[4], want[5], "{state:?}");
            }
        }
    }
}
