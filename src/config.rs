//! Config file schema for lcm-status. Plain TOML, hand-editable, lives at
//! /etc/lcm-status.toml by default. Every field has a sensible default so
//! the daemon runs fine with no config file at all.

use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/lcm-status.toml";

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub display: DisplayConfig,
    pub rotation: RotationConfig,
    pub menu: MenuConfig,
    pub sleep: SleepConfig,
    pub socket: SocketConfig,
    pub screens: ScreensConfig,
    pub refresh: RefreshConfig,
    pub temperature: TemperatureConfig,
    pub docker: DockerConfig,
    pub led: LedConfig,
    /// One entry per physical fan to control -- see `FanProfile`. Defaults
    /// to this board's single real fan (`default_fans()` below) so the
    /// daemon behaves the same with no config file at all; an explicit
    /// `[[fans]]` in the config file replaces the defaults entirely rather
    /// than merging with them (matches how a fresh `pwmconfig` run replaces
    /// `/etc/fancontrol` wholesale, not field-by-field).
    #[serde(default = "default_fans")]
    pub fans: Vec<FanProfile>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            display: DisplayConfig::default(),
            rotation: RotationConfig::default(),
            menu: MenuConfig::default(),
            sleep: SleepConfig::default(),
            socket: SocketConfig::default(),
            screens: ScreensConfig::default(),
            refresh: RefreshConfig::default(),
            temperature: TemperatureConfig::default(),
            docker: DockerConfig::default(),
            led: LedConfig::default(),
            fans: default_fans(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DisplayConfig {
    /// Serial device the LCM MCU is on.
    pub serial_device: String,
    /// Milliseconds per character-step when scrolling a line that overflows
    /// 16 characters. ~300ms (~3.3 chars/sec) is the legible sweet spot;
    /// lower = faster/harder to read, higher = sluggish.
    pub scroll_step_ms: u64,
    /// Max characters kept for a scrolling line before truncating. Bounds
    /// worst-case scroll-cycle time; realistic content (container names,
    /// pool names, alert text) fits comfortably under this.
    pub scroll_max_chars: usize,
    /// Pause (ms) at the start of a scroll cycle before it begins moving,
    /// and again at the end before it loops, so a short-lived viewer isn't
    /// mid-scroll the whole time they glance at the panel.
    pub scroll_pause_ms: u64,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            serial_device: "/dev/ttyS1".to_string(),
            scroll_step_ms: 300,
            scroll_max_chars: 64,
            scroll_pause_ms: 800,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RotationConfig {
    /// How long a non-scrolling (short) screen is shown before auto-advancing.
    pub dwell_secs: u64,
    /// After a manual UP/DOWN page, how long before auto-rotation resumes.
    pub resume_after_secs: u64,
}

impl Default for RotationConfig {
    fn default() -> Self {
        RotationConfig {
            dwell_secs: 5,
            resume_after_secs: 30,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MenuConfig {
    /// Auto-cancel back to rotation if a confirm screen (shutdown/restart/eject)
    /// gets no input for this many seconds.
    pub confirm_timeout_secs: u64,
}

impl Default for MenuConfig {
    fn default() -> Self {
        MenuConfig {
            confirm_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SleepConfig {
    pub enabled: bool,
    /// 24h "HH:MM" local time.
    pub start: String,
    pub end: String,
    /// sysfs LED-class brightness file controlling real backlight power.
    /// Confirmed on AS6704T v2 as /sys/class/leds/power:lcd/brightness;
    /// override if a different model exposes it under another name.
    pub lcd_power_path: String,
}

impl Default for SleepConfig {
    fn default() -> Self {
        SleepConfig {
            enabled: false,
            start: "23:00".to_string(),
            end: "07:00".to_string(),
            lcd_power_path: "/sys/class/leds/power:lcd/brightness".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LedConfig {
    /// "link": front LED solid while connected (factory default).
    /// "activity": dark at idle, flashes on traffic.
    /// Reuses [sleep]'s schedule for LED night mode too -- one schedule
    /// governs both the LCD backlight and the front LEDs, not two.
    pub nic_mode: crate::led::NicLedMode,
}

impl Default for LedConfig {
    fn default() -> Self {
        LedConfig { nic_mode: crate::led::NicLedMode::Link }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SocketConfig {
    pub path: String,
    /// Group allowed to connect (e.g. so another daemon like an LED
    /// controller can push text without running as root).
    pub group: String,
}

impl Default for SocketConfig {
    fn default() -> Self {
        SocketConfig {
            path: "/run/lcm-status.sock".to_string(),
            group: "lcm-status".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ScreensConfig {
    pub network: bool,
    /// Pool capacity + health, merged into one screen per pool.
    pub pools: bool,
    pub hdd: bool,
    pub temperature: bool,
    pub docker: bool,
}

impl Default for ScreensConfig {
    fn default() -> Self {
        ScreensConfig {
            network: true,
            pools: true,
            hdd: true,
            temperature: true,
            docker: true,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RefreshConfig {
    /// Minimum seconds between refreshes for each category, even if the
    /// rotation loop comes back around faster. Refresh is otherwise
    /// pull-based (done right before a screen is (re)displayed), so these
    /// are floors, not fixed polling intervals.
    pub network_min_secs: u64,
    pub pools_min_secs: u64,
    pub hdd_min_secs: u64,
    pub temperature_min_secs: u64,
    pub docker_min_secs: u64,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        RefreshConfig {
            network_min_secs: 10,
            pools_min_secs: 30,
            hdd_min_secs: 300, // SMART: avoid waking spun-down drives too often
            temperature_min_secs: 10,
            docker_min_secs: 10,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TemperatureConfig {
    pub units: TempUnits,
    /// Highlight (and let alerts trigger on) temps at or above this, in
    /// whichever unit `units` is set to.
    pub warn_threshold: f32,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TempUnits {
    C,
    F,
}

impl Default for TemperatureConfig {
    fn default() -> Self {
        TemperatureConfig {
            units: TempUnits::C,
            warn_threshold: 60.0,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DockerConfig {
    /// Container names to ignore entirely when checking health (e.g. known
    /// noisy/expected-unhealthy containers).
    pub ignore: Vec<String>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        DockerConfig { ignore: Vec::new() }
    }
}

/// One physical fan: which pwm output drives it, which sensors feed its
/// curve, and the curve itself. Config shape deliberately mirrors what
/// `fancontrol`/`pwmconfig` expose (one curve + one sensor set per pwm
/// output, `[[fans]]` here playing the role of `FCTEMPS`/`FCFANS`/
/// `MINTEMP`/etc per device in `/etc/fancontrol`), plus this project's own
/// enhancement: `sensors` is a *list*, and the control temp is the max
/// across all of them, not one fixed sensor. Multiple `[[fans]]` blocks
/// support boards with more than one controllable fan; run `lcm-status
/// fan-profile` (see `fan_calibrate.rs`) to discover what's actually
/// connected on a given board rather than guessing.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FanProfile {
    /// Just a label for logging -- doesn't need to be unique, though it's
    /// clearer if it is.
    pub name: String,
    pub enabled: bool,
    /// hwmon chip name (the `name` sysfs file's content, e.g. "it8625")
    /// that exposes the pwm output. Resolved fresh at runtime, same as
    /// sensor chips -- hwmon numbers aren't stable across reloads.
    pub pwm_chip: String,
    /// Which pwm output on that chip, e.g. 1 for `pwm1`/`pwm1_enable`.
    pub pwm_index: u32,
    /// Which tachometer input reports this fan's RPM, e.g. 1 for
    /// `fan1_input` -- used for stall detection (see fan.rs). `None` if
    /// this pwm output has no associated tach (some boards genuinely don't
    /// wire one up); stall detection then falls back to "pwm was
    /// commanded to 0" only, since there's no RPM to check.
    pub fan_index: Option<u32>,
    /// How often to re-evaluate this fan's curve and (re)write its pwm.
    /// lm-sensors' fancontrol(8) default INTERVAL is also 1s.
    pub update_secs: u64,
    /// At or below this control temp, the fan is pinned to min_pwm.
    pub min_temp_c: f32,
    /// At or above this control temp, the fan is pinned to max_pwm.
    pub max_temp_c: f32,
    /// PWM needed to reliably get a *stopped* fan spinning again. Below
    /// this, a stopped fan stays stopped rather than crawl at a PWM too low
    /// to actually start it turning. `lcm-status fan-profile` measures this
    /// empirically per fan rather than guessing.
    pub min_start_pwm: u8,
    /// Once running, the fan is allowed to coast down to this PWM before
    /// it's allowed to stop entirely (also the ramp's value at min_temp_c
    /// -- see fan.rs for why that's not min_pwm; same as upstream
    /// fancontrol).
    pub min_stop_pwm: u8,
    /// PWM used flat at/below min_temp_c.
    pub min_pwm: u8,
    /// PWM used flat at/above max_temp_c. 255 = fully on.
    pub max_pwm: u8,
    /// Which sensors feed this fan's control temp -- the max of all of
    /// them, not just one. Empty means this fan never sees a temp reading,
    /// which effectively disables it (compute_pwm has nothing to act on).
    pub sensors: Vec<SensorSelector>,
}

impl Default for FanProfile {
    fn default() -> Self {
        // Curve defaults match this board's tuned values, so a partial
        // `[[fans]]` entry (e.g. just overriding pwm_chip/sensors) still
        // gets a sane curve. pwm_chip/sensors themselves default to empty
        // -- a profile without them is a meaningless no-op (fan.rs skips a
        // fan with no sensors resolved), which is a fine failure mode for
        // "you forgot to fill this in" rather than silently controlling
        // the wrong chip.
        FanProfile {
            name: "fan".to_string(),
            enabled: true,
            pwm_chip: String::new(),
            pwm_index: 1,
            fan_index: None,
            update_secs: 1,
            min_temp_c: 45.0,
            max_temp_c: 90.0,
            min_start_pwm: 60,
            min_stop_pwm: 55,
            min_pwm: 50,
            max_pwm: 255,
            sensors: Vec::new(),
        }
    }
}

/// Identifies one or more hwmon temperature inputs. `chip` alone (the most
/// common case) matches every temp input on every hwmon instance of that
/// chip -- e.g. `chip = "drivetemp"` picks up every populated drive bay,
/// however many there are, without needing to enumerate them. Narrow with
/// `input`/`label` for a chip that exposes several temps and only one (or
/// a specific subset) should count, e.g. `it8625`'s temp1/2/3 (most of
/// which are unwired on this board -- see `min_resample_secs` note below
/// on why that's handled by filtering, not by guessing which to list) or
/// `coretemp`'s per-core inputs alongside its package input.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SensorSelector {
    /// hwmon chip name, e.g. "coretemp", "drivetemp", "nvme", "it8625".
    pub chip: String,
    /// A specific sysfs attribute base name, e.g. "temp2" (for
    /// `temp2_input`). If unset, matches every `tempN_input` the chip
    /// exposes.
    pub input: Option<String>,
    /// Substring match against `tempN_label`, for a chip with several
    /// inputs where only some should count (e.g. `label = "Package"` on
    /// coretemp, to use the package temp and not individual cores).
    /// Inputs with no label file never match a selector that sets this.
    pub label: Option<String>,
    /// Overrides the owning fan's `update_secs` for *this* sensor's
    /// resample rate. Leave unset to just use `update_secs` (fine for CPU,
    /// which can change quickly); set higher for slow-changing sensors
    /// (drive/NVMe temps) so they aren't reread every tick for no reason.
    /// A disconnected sensor (fault flag set, or an implausible reading --
    /// see `hal::read_temp_input`) is silently excluded rather than
    /// treated as 0 or as an error, the same way an unpopulated drive bay
    /// simply has no `drivetemp` hwmon instance at all.
    pub min_resample_secs: Option<u64>,
}

impl Default for SensorSelector {
    fn default() -> Self {
        SensorSelector { chip: String::new(), input: None, label: None, min_resample_secs: None }
    }
}

/// This board's single real fan (AS6704T: `it8625` `pwm1`/`fan1`; fan2/3
/// headers exist on the chip but nothing's physically connected -- see
/// `hal::cpu_and_fan`'s doc comment). Used as `Config`'s default so the
/// daemon needs no `[[fans]]` config at all to behave the way it always
/// has; curve values match the `/etc/fancontrol` config this replaced.
pub fn default_fans() -> Vec<FanProfile> {
    vec![FanProfile {
        name: "chassis".to_string(),
        enabled: true,
        pwm_chip: "it8625".to_string(),
        pwm_index: 1,
        fan_index: Some(1),
        update_secs: 1,
        min_temp_c: 45.0,
        max_temp_c: 90.0,
        min_start_pwm: 60,
        min_stop_pwm: 55,
        min_pwm: 50,
        max_pwm: 255,
        sensors: vec![
            SensorSelector { chip: "coretemp".to_string(), label: Some("Package".to_string()), ..Default::default() },
            SensorSelector { chip: "drivetemp".to_string(), min_resample_secs: Some(30), ..Default::default() },
            SensorSelector { chip: "nvme".to_string(), min_resample_secs: Some(30), ..Default::default() },
        ],
    }]
}

impl Config {
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("failed to parse {}: {e}, using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }
}
