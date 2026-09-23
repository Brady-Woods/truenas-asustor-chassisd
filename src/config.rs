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
    pub fan: FanConfig,
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
            fan: FanConfig::default(),
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

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FanConfig {
    pub enabled: bool,
    /// How often to re-evaluate the curve and (re)write pwm1. lm-sensors'
    /// fancontrol(8) default INTERVAL is also 1s; carried over unchanged.
    pub update_secs: u64,
    /// At or below this CPU temp, the fan is allowed to be fully stopped.
    pub min_temp_c: f32,
    /// At or above this CPU temp, pwm1 is pinned to max_pwm.
    pub max_temp_c: f32,
    /// PWM needed to reliably get a *stopped* fan spinning again. Below
    /// this, a stopped fan stays stopped rather than crawl at a PWM too low
    /// to actually start it turning.
    pub min_start_pwm: u8,
    /// Once running, the fan is allowed to coast down to this PWM before
    /// it's allowed to stop entirely (prevents rapid stop/start cycling
    /// right at the boundary).
    pub min_stop_pwm: u8,
    /// Floor once the fan is running (the curve's PWM at min_temp_c).
    pub min_pwm: u8,
    /// Ceiling (the curve's PWM at max_temp_c). 255 = fully on.
    pub max_pwm: u8,
    /// How often to resample drive/NVMe temps for the control-temp max
    /// (separate from `update_secs`, which governs the CPU reading and the
    /// pwm1 write). Drive temps change on a much slower timescale than CPU
    /// load, and this reads via /sys/class/hwmon directly (no smartctl),
    /// but there's no reason to hammer it every second either.
    pub drive_temp_min_secs: u64,
}

impl Default for FanConfig {
    fn default() -> Self {
        // These match the /etc/fancontrol curve this replaces, hand-tuned
        // on nas.skycorgi.net's AS6704T (it8625 pwm1, driven by coretemp
        // package temp -- the chip's own thermal inputs are unconnected on
        // this board).
        FanConfig {
            enabled: true,
            update_secs: 1,
            min_temp_c: 45.0,
            max_temp_c: 90.0,
            min_start_pwm: 60,
            min_stop_pwm: 55,
            min_pwm: 50,
            max_pwm: 255,
            drive_temp_min_secs: 30,
        }
    }
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
