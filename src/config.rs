//! Config file schema for lcm-status. Plain TOML, hand-editable, lives at
//! /etc/lcm-status.toml by default. Every field has a sensible default so
//! the daemon runs fine with no config file at all.

use serde::{Deserialize, Deserializer};
use std::fmt::Write as _;
use std::path::Path;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/lcm-status.toml";

/// Upper bound for every user-set duration in seconds that ends up in
/// `Instant + Duration` arithmetic (a day is longer than any sensible
/// dwell, timeout or refresh floor). Anything larger is clamped with a
/// diagnostic; `state::deadline_after` is the second line of defence.
pub const MAX_SECS: u64 = 86_400;
/// Longest scrolling line (characters) and end-to-start gap accepted.
const MAX_SCROLL_CHARS: usize = 1024;
const MAX_SCROLL_GAP: usize = 64;

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
    pub network: NetworkConfig,
    pub wol: WolConfig,
    pub cpu_power: CpuPowerConfig,
    pub buzzer: BuzzerConfig,
    pub templates: TemplatesConfig,
    /// One entry per physical fan to control -- see `FanProfile`. Defaults
    /// to this board's single real fan (`default_fans()` below) so the
    /// daemon behaves the same with no config file at all; an explicit
    /// `[[fans]]` in the config file replaces the defaults entirely rather
    /// than merging with them (matches how a fresh `pwmconfig` run replaces
    /// `/etc/fancontrol` wholesale, not field-by-field).
    #[serde(default = "default_fans")]
    pub fans: Vec<FanProfile>,
    /// Weekly power on / shutdown / restart rules, ADM-style -- see
    /// `PowerRule` and `power.rs`. Empty (default) = no schedule, and the
    /// RTC wake alarm is never touched.
    pub power_schedule: Vec<PowerRule>,
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
            network: NetworkConfig::default(),
            wol: WolConfig::default(),
            cpu_power: CpuPowerConfig::default(),
            buzzer: BuzzerConfig::default(),
            templates: TemplatesConfig::default(),
            fans: default_fans(),
            power_schedule: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct NetworkConfig {
    /// Whether network link state feeds the status LED at all. Set false
    /// to go back to pool-health/socket-overrides only.
    pub enabled: bool,
    /// Which interfaces' link state counts for status-LED purposes. Empty
    /// (default) means "whatever currently has an IP" (`hal::configured_nics`)
    /// -- auto-adjusts as interfaces are configured/unconfigured. Set
    /// explicitly to pin the list (e.g. if you want a specific NIC
    /// monitored even before it has an address yet, or want to exclude one
    /// that does).
    pub monitored_nics: Vec<String>,
    /// Status-LED severity when *some* (but not all) monitored NICs are
    /// down -- link failure on a redundant/bonded setup is degraded, not
    /// necessarily an outage.
    pub some_down_level: NetworkLevel,
    /// Status-LED severity when *every* monitored NIC is down -- normally
    /// worse than "some", since that's a real total network outage.
    pub all_down_level: NetworkLevel,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            enabled: true,
            monitored_nics: Vec::new(),
            some_down_level: NetworkLevel::Warning,
            all_down_level: NetworkLevel::Error,
        }
    }
}

/// The chassis beeper (ADM's "enable buzzer"), see `buzzer.rs`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
#[expect(clippy::struct_excessive_bools, reason = "one on/off key per beep")]
pub struct BuzzerConfig {
    /// Master switch. Off by default: a box that suddenly beeps is a
    /// surprise nobody asked for.
    pub enabled: bool,
    /// Beep once (long) when the daemon starts during boot.
    pub boot: bool,
    /// Beep (short) just before a shutdown or restart.
    pub power: bool,
    /// Beep (short) for a socket `SHOW` that beeps (by default error once,
    /// critical again every minute until cleared; `beep=` overrides). Arrival
    /// beeps are at most once a minute; during the sleep window only critical.
    pub alerts: bool,
    /// Beep (short) when a chassis `LOCATE` starts.
    pub find_me: bool,
}

impl Default for BuzzerConfig {
    fn default() -> Self {
        BuzzerConfig {
            enabled: false,
            boot: true,
            power: true,
            alerts: true,
            find_me: true,
        }
    }
}

/// Wake-on-LAN, kept applied by `wol::WolKeeper`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct WolConfig {
    /// Interfaces to keep Wake-on-LAN enabled on, e.g. `["enp2s0"]`. Empty
    /// (default) means WOL isn't touched at all -- whatever the driver
    /// came up with stays.
    pub nics: Vec<String>,
    /// `ethtool`-style wake flags (`ethtool -s IFACE wol <mode>`): `"g"`
    /// (default) = magic packet; also `p`/`u`/`m`/`b`/`a`, or `"d"` to
    /// keep WOL *off* on those NICs. Replaces the NIC's whole setting
    /// rather than adding to it, same as `ethtool`.
    pub mode: String,
}

impl Default for WolConfig {
    fn default() -> Self {
        WolConfig {
            nics: Vec::new(),
            mode: "g".to_string(),
        }
    }
}

/// CPU package power limits (Intel RAPL), kept applied by
/// `cpu_power::CpuPowerService` and dropped back to `stock_*` while any
/// temperature sensor is critical. Whole watts and seconds: the kernel
/// takes microwatts/microseconds and this avoids float-to-int casts.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CpuPowerConfig {
    /// Off by default: the daemon never touches the CPU's power limits
    /// unless asked.
    pub enabled: bool,
    /// PL1, the sustained limit, in watts.
    pub pl1_w: u32,
    /// PL2, the burst limit, in watts. At least `pl1_w`.
    pub pl2_w: u32,
    /// How long, in seconds, PL1 averages power over (the burst window).
    pub tau_secs: u32,
    /// What PL1/PL2/tau are put back to while a sensor is critical. The
    /// defaults are this board's (Celeron N5105) firmware values.
    pub stock_pl1_w: u32,
    pub stock_pl2_w: u32,
    pub stock_tau_secs: u32,
    /// Seconds every sensor must stay under its *warning* threshold
    /// before the configured limits come back after a critical.
    pub rearm_secs: u64,
    /// Seconds between temperature sweeps.
    pub check_secs: u64,
}

impl Default for CpuPowerConfig {
    fn default() -> Self {
        CpuPowerConfig {
            enabled: false,
            pl1_w: 12,
            pl2_w: 14,
            tau_secs: 120,
            stock_pl1_w: 10,
            stock_pl2_w: 25,
            stock_tau_secs: 28,
            rearm_secs: 60,
            check_secs: 5,
        }
    }
}

/// Highest power limit accepted, in watts: well above anything a NAS
/// `SoC` takes, low enough to catch a milliwatt/microwatt slip.
pub const MAX_CPU_POWER_W: u32 = 100;
/// Longest accepted PL1 window, in seconds.
pub const MAX_CPU_TAU_SECS: u32 = 3600;
/// Accepted range for `[cpu_power] check_secs`.
pub const CPU_POWER_CHECK_SECS: std::ops::RangeInclusive<u64> = 1..=60;

impl CpuPowerConfig {
    /// Everything wrong with this section, one message each.
    fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, pl1, pl2, tau) in [
            ("", self.pl1_w, self.pl2_w, self.tau_secs),
            (
                "stock_",
                self.stock_pl1_w,
                self.stock_pl2_w,
                self.stock_tau_secs,
            ),
        ] {
            if !(1..=MAX_CPU_POWER_W).contains(&pl1) || !(1..=MAX_CPU_POWER_W).contains(&pl2) {
                out.push(format!(
                    "{name}pl1_w/{name}pl2_w must be 1..={MAX_CPU_POWER_W} (got {pl1}/{pl2})"
                ));
            } else if pl2 < pl1 {
                out.push(format!("{name}pl2_w ({pl2}) is below {name}pl1_w ({pl1})"));
            }
            if !(1..=MAX_CPU_TAU_SECS).contains(&tau) {
                out.push(format!(
                    "{name}tau_secs must be 1..={MAX_CPU_TAU_SECS} (got {tau})"
                ));
            }
        }
        if !CPU_POWER_CHECK_SECS.contains(&self.check_secs) {
            out.push(format!(
                "check_secs must be {}..={} (got {})",
                CPU_POWER_CHECK_SECS.start(),
                CPU_POWER_CHECK_SECS.end(),
                self.check_secs
            ));
        }
        if self.rearm_secs > MAX_SECS {
            out.push(format!(
                "rearm_secs = {} is over {MAX_SECS}",
                self.rearm_secs
            ));
        }
        out
    }
}

/// `crate::socket::Level`'s Warn/Error, spelled out for this config context
/// (Info/Critical aren't sensible choices here: Info wouldn't show on the
/// LED at all, and a link failure alone -- unlike a stalled fan or a
/// pool actually gone -- isn't the "flash red" tier of emergency by
/// default; set both to the same value if you disagree for your setup).
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkLevel {
    Warning,
    Error,
}

impl From<NetworkLevel> for crate::socket::Level {
    fn from(l: NetworkLevel) -> Self {
        match l {
            NetworkLevel::Warning => crate::socket::Level::Warn,
            NetworkLevel::Error => crate::socket::Level::Error,
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
    /// Blank characters between the end of a scrolling line and its start
    /// coming back around.
    pub scroll_gap: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            serial_device: crate::protocol::LCM_DEVICE.to_string(),
            scroll_step_ms: 300,
            scroll_max_chars: 64,
            scroll_pause_ms: 800,
            scroll_gap: 4,
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
    /// Auto-cancel back to rotation if a confirm screen (shutdown/restart)
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
    /// Switch the LCD's display (backlight included) off while asleep,
    /// rather than only blanking its text and leaving the backlight glow.
    /// Uses the panel's own display-off command, not the LCD power rail
    /// (the platform driver's `lcd_power`, never written here), so the
    /// panel's MCU stays powered and a button press still wakes it.
    pub lcd_off: bool,
}

impl Default for SleepConfig {
    fn default() -> Self {
        SleepConfig {
            enabled: false,
            start: "23:00".to_string(),
            end: "07:00".to_string(),
            lcd_off: true,
        }
    }
}

/// One `[[power_schedule]]` entry: do `action` at `time` on each of
/// `days`. Mirrors ADM's power schedule rules (type, days of week, hour,
/// minute). Kept as plain strings here and checked by `Config::validate`
/// (via `power::Rule::parse`), so one bad entry is reported and disabled
/// rather than failing the whole file the way a bad enum value would.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PowerRule {
    pub enabled: bool,
    /// Day names ("mon", "monday", ...) and/or "daily", "weekdays",
    /// "weekends". A single string is accepted as a one-item list.
    #[serde(deserialize_with = "one_or_many")]
    pub days: Vec<String>,
    /// 24h "HH:MM" local time.
    pub time: String,
    /// "`power_on`" (RTC wake from soft-off), "shutdown" or "restart".
    /// ADM's "`power_off`" is accepted for "shutdown".
    pub action: String,
    /// shutdown/restart only: how long the front panel counts down (any
    /// button cancels) before it happens. 0 = immediately.
    pub countdown_secs: u64,
}

impl Default for PowerRule {
    fn default() -> Self {
        PowerRule {
            enabled: true,
            days: Vec::new(),
            time: String::new(),
            action: String::new(),
            countdown_secs: 60,
        }
    }
}

/// A string or a list of strings, as a list.
fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LedConfig {
    /// "link": front LED solid while connected (factory default).
    /// "activity": dark at idle, flashes on traffic.
    /// Reuses [sleep]'s schedule for LED night mode too -- one schedule
    /// governs both the LCD backlight and the front LEDs, not two.
    pub nic_mode: crate::led::NicLedMode,
    /// Front LED brightness, 0-100 (power, status, LAN, USB -- not the bay
    /// LEDs, which have no brightness control), through the
    /// `front_panel::brightness` LED (`it87` with `led_pwm=3
    /// led_pwm_invert=1`). Unset leaves the BIOS level alone (about 80%).
    pub brightness: Option<u8>,
    /// Brightness for those same LEDs during night mode, 0-100. Unset
    /// (default) switches them off at night instead; set, they keep
    /// showing status, just dimmed. Bay green LEDs go dark either way.
    pub night_brightness: Option<u8>,
    /// "ready": a bay's green LED is solid while a disk is present and
    /// blinks off on access (factory). "activity": dark at idle, flashes on
    /// access. Unset leaves the driver's setting alone. Needs the platform
    /// driver's runtime-writable `disk_led_ready`.
    pub bay_mode: Option<crate::led::BayLedMode>,
}

impl Default for LedConfig {
    fn default() -> Self {
        LedConfig {
            nic_mode: crate::led::NicLedMode::Link,
            brightness: None,
            night_brightness: None,
            bay_mode: None,
        }
    }
}

/// TrueNAS SCALE's middleware always provides this group (gid 544) and
/// regenerates `/etc/group` from it at boot, so no purpose-made group is
/// needed.
pub const DEFAULT_SOCKET_GROUP: &str = "builtin_administrators";

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SocketConfig {
    pub path: String,
    /// Group allowed to connect (e.g. so another daemon like an LED
    /// controller can push text without running as root). Defaults to
    /// TrueNAS's always-present `builtin_administrators`; if the named
    /// group doesn't exist the socket stays root-only.
    pub group: String,
}

impl Default for SocketConfig {
    fn default() -> Self {
        SocketConfig {
            path: "/run/lcm-status.sock".to_string(),
            group: DEFAULT_SOCKET_GROUP.to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
#[expect(clippy::struct_excessive_bools, reason = "one on/off key per screen")]
pub struct ScreensConfig {
    pub network: bool,
    /// Pool capacity + health, merged into one screen per pool.
    pub pools: bool,
    pub hdd: bool,
    pub temperature: bool,
    pub docker: bool,
    /// Rotation order of the screen categories. Any category left out is
    /// appended in the default order, so listing just `["docker"]` means
    /// "docker first, everything else as usual" -- whether a category is
    /// shown at all is still the bool flags above, not this list.
    pub order: Vec<Category>,
}

impl Default for ScreensConfig {
    fn default() -> Self {
        ScreensConfig {
            network: true,
            pools: true,
            hdd: true,
            temperature: true,
            docker: true,
            order: Category::ALL.to_vec(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Network,
    Pools,
    Hdd,
    Temperature,
    Docker,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Network,
        Category::Pools,
        Category::Hdd,
        Category::Temperature,
        Category::Docker,
    ];
}

impl ScreensConfig {
    /// `order` with duplicates dropped and missing categories appended.
    pub fn effective_order(&self) -> Vec<Category> {
        let mut out: Vec<Category> = Vec::new();
        for c in self.order.iter().chain(Category::ALL.iter()) {
            if !out.contains(c) {
                out.push(*c);
            }
        }
        out
    }
}

/// Two-line text for one kind of status screen -- see `TemplatesConfig`.
/// A block that sets only one line leaves the other blank; leaving the
/// whole block out is what keeps the defaults.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ScreenTemplate {
    pub line0: String,
    pub line1: String,
}

impl ScreenTemplate {
    fn new(line0: &str, line1: &str) -> Self {
        ScreenTemplate {
            line0: line0.to_string(),
            line1: line1.to_string(),
        }
    }
}

/// LCD text for each status screen, as `{var}` templates (see
/// `template.rs`). Defaults reproduce the built-in text exactly. Each
/// screen kind has its own fixed set of variables (`VARS` below); a
/// template using anything else is rejected at load and that one screen
/// falls back to its default. Fallback screens ("no disks found", ...),
/// socket overrides, and the action menu aren't templated.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TemplatesConfig {
    pub network: ScreenTemplate,
    pub pool: ScreenTemplate,
    pub hdd: ScreenTemplate,
    pub cpu: ScreenTemplate,
    pub fan: ScreenTemplate,
    pub docker: ScreenTemplate,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        TemplatesConfig {
            network: ScreenTemplate::new("{iface}", "{ip_or_status}"),
            pool: ScreenTemplate::new("{name}: {health}", "{alloc}/{size} {cap}"),
            hdd: ScreenTemplate::new("{label}", "{status} {temp}"),
            cpu: ScreenTemplate::new("CPU", "{temp}{unit}{warn}"),
            fan: ScreenTemplate::new("FAN", "{rpm} RPM"),
            docker: ScreenTemplate::new("DOCKER {name}", "{status}"),
        }
    }
}

impl TemplatesConfig {
    /// Variables each screen kind provides -- must match what the
    /// corresponding `hal.rs` function passes to `template::render`.
    pub const NETWORK_VARS: &[&str] = &["iface", "ip", "link", "ip_or_status"];
    pub const POOL_VARS: &[&str] = &["name", "size", "alloc", "free", "cap", "health"];
    pub const HDD_VARS: &[&str] = &["label", "bay", "dev", "status", "temp"];
    pub const CPU_VARS: &[&str] = &["temp", "unit", "warn"];
    pub const FAN_VARS: &[&str] = &["rpm"];
    pub const DOCKER_VARS: &[&str] = &["name", "status"];

    /// Replaces any template that fails to parse or uses an unknown
    /// variable with its default, returning one message per replacement.
    /// Per-screen rather than all-or-nothing so one typo doesn't throw
    /// away every other customization.
    pub fn validate(&mut self) -> Vec<String> {
        let defaults = TemplatesConfig::default();
        let mut errors = Vec::new();
        let mut check =
            |kind: &str, tpl: &mut ScreenTemplate, default: ScreenTemplate, vars: &[&str]| {
                let problem = [&tpl.line0, &tpl.line1].into_iter().find_map(|line| {
                    match crate::template::placeholders(line) {
                        Err(e) => Some(e),
                        Ok(names) => names.into_iter().find(|n| !vars.contains(n)).map(|n| {
                            format!(
                                "unknown variable {{{n}}} in \"{line}\" (available: {})",
                                vars.join(", ")
                            )
                        }),
                    }
                });
                if let Some(p) = problem {
                    errors.push(format!("[templates.{kind}]: {p}; using the default"));
                    *tpl = default;
                }
            };
        check(
            "network",
            &mut self.network,
            defaults.network,
            Self::NETWORK_VARS,
        );
        check("pool", &mut self.pool, defaults.pool, Self::POOL_VARS);
        check("hdd", &mut self.hdd, defaults.hdd, Self::HDD_VARS);
        check("cpu", &mut self.cpu, defaults.cpu, Self::CPU_VARS);
        check("fan", &mut self.fan, defaults.fan, Self::FAN_VARS);
        check(
            "docker",
            &mut self.docker,
            defaults.docker,
            Self::DOCKER_VARS,
        );
        errors
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
#[expect(clippy::struct_field_names, reason = "field names are the config keys")]
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
    /// Fallback for any sensor not matched by `thresholds` below. Highlight
    /// on the temperature screen, and log a syslog WARNING
    /// (`monitor::HealthMonitor`), for any currently-connected sensor at or
    /// above this. Always degrees C regardless of `units` (which only
    /// affects the CPU and HDD *screens'* display, not this comparison).
    pub warn_threshold: f32,
    /// Fallback critical threshold -- log CRITICAL instead of WARNING for
    /// any sensor at or above this. Always degrees C.
    pub critical_threshold: f32,
    /// Per-chip overrides -- a CPU, an HDD, and an NVMe SSD have very
    /// different safe operating ranges, so one global pair is a blunt
    /// instrument. Matched by `chip` (the hwmon `name` file's content,
    /// e.g. "coretemp", "drivetemp", "nvme") against every sensor that
    /// chip exposes; a chip with no matching entry here falls back to
    /// `warn_threshold`/`critical_threshold` above. See `default_temp_thresholds()`
    /// for this hardware's defaults and the research behind them.
    pub thresholds: Vec<TempThresholdOverride>,
}

/// One chip's warn/critical pair -- see `TemperatureConfig::thresholds`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TempThresholdOverride {
    pub chip: String,
    pub warn_threshold: f32,
    pub critical_threshold: f32,
}

impl Default for TempThresholdOverride {
    fn default() -> Self {
        TempThresholdOverride {
            chip: String::new(),
            warn_threshold: 75.0,
            critical_threshold: 85.0,
        }
    }
}

/// This board's per-chip thresholds, sourced from each component's actual
/// datasheet where one was publicly available (2026-09-23) rather than
/// guessed:
///
/// - **coretemp** (Intel Celeron N5105): Intel ARK lists TjMax (T
///   Junction, the throttle/shutdown point) at 105C. Warn at 85C, critical
///   at 100C -- both comfortably under TjMax, critical close enough to it
///   to mean "throttling is imminent or already happening", not a
///   hypothetical.
/// - **drivetemp** (WD Ultrastar DC HC550, this box's 4 bays): WD's own
///   datasheet lists operating temperature 5-60C, with MTBF/AFR derating
///   starting above 40C ambient and their own worst-case reference point
///   being 60C ambient / 65C device temp. Warn at 50C (comfortably into
///   the derating zone but well short of the limit), critical at 60C
///   (the drive's own spec'd operating ceiling).
/// - **nvme** (WD Black SN750): WD's datasheet lists operating temperature
///   (their "composite temperature", the same value this chip's `nvme`
///   hwmon reports) as 0-70C. Warn at 60C, critical at 70C -- the spec's
///   own ceiling, not a margin below it, since composite temp already *is*
///   the number WD says not to exceed.
///
/// - **enp9s0** (Marvell/Aquantia AQC113 10GbE card, `atlantic` driver):
///   its PHY and MAC temperature sensors share one hwmon device that the
///   driver names after the interface, so this is the card's interface name
///   on this board. No public datasheet with a numeric limit was found, so
///   these are ADM's own LAN-chip temperature curve for platforms that read
///   one (70/80/100C, emergency above 101C; see the teardown's
///   `emboardmand.md`): warn at 80C, critical at 100C.
pub fn default_temp_thresholds() -> Vec<TempThresholdOverride> {
    vec![
        TempThresholdOverride {
            chip: "coretemp".to_string(),
            warn_threshold: 85.0,
            critical_threshold: 100.0,
        },
        TempThresholdOverride {
            chip: "drivetemp".to_string(),
            warn_threshold: 50.0,
            critical_threshold: 60.0,
        },
        TempThresholdOverride {
            chip: "nvme".to_string(),
            warn_threshold: 60.0,
            critical_threshold: 70.0,
        },
        TempThresholdOverride {
            chip: "enp9s0".to_string(),
            warn_threshold: 80.0,
            critical_threshold: 100.0,
        },
    ]
}

/// (warn, critical) for a given hwmon chip name -- its `thresholds`
/// override if one matches, else the global fallback pair. Shared by
/// `monitor::HealthMonitor::check_temps` and the `status` report
/// (`report.rs`) so both apply exactly the same resolution, not two
/// copies that could drift.
pub fn resolve_temp_threshold(cfg: &TemperatureConfig, chip: &str) -> (f32, f32) {
    cfg.thresholds
        .iter()
        .find(|t| t.chip == chip)
        .map_or((cfg.warn_threshold, cfg.critical_threshold), |t| {
            (t.warn_threshold, t.critical_threshold)
        })
}

/// A temperature as the startup summary prints it: `90`, `82.5`.
fn fmt_c(v: f32) -> String {
    format!("{v}C")
}

/// The effective warn/critical pairs for the startup log: each chip
/// override, then the fallback that every other chip gets (labelled so it
/// isn't read as applying to the overridden ones).
pub fn describe_thresholds(cfg: &TemperatureConfig) -> String {
    let mut parts: Vec<String> = cfg
        .thresholds
        .iter()
        .map(|t| {
            format!(
                "{} {}/{}",
                t.chip,
                fmt_c(t.warn_threshold),
                fmt_c(t.critical_threshold)
            )
        })
        .collect();
    parts.push(format!(
        "other chips {}/{}",
        fmt_c(cfg.warn_threshold),
        fmt_c(cfg.critical_threshold)
    ));
    parts.join(", ")
}

/// Every connected temperature sensor with its reading and the thresholds
/// it is actually judged against, for one startup log line.
pub fn describe_sensors(cfg: &TemperatureConfig, temps: &[(String, String, f32)]) -> String {
    if temps.is_empty() {
        return "sensors: none detected".to_string();
    }
    let items: Vec<String> = temps
        .iter()
        .map(|(chip, desc, t)| {
            let (warn, crit) = resolve_temp_threshold(cfg, chip);
            format!(
                "{desc} {t:.0}C (warn {}, crit {})",
                fmt_c(warn),
                fmt_c(crit)
            )
        })
        .collect();
    format!("sensors: {}", items.join("; "))
}

/// One fan's control settings for the startup log: where it is wired, how
/// its pwm is chosen, and the curve each of its sensors feeds.
pub fn describe_fan(f: &FanProfile) -> String {
    let tach = f
        .fan_index
        .map_or_else(|| "no tach".to_string(), |i| format!("tach fan{i}"));
    let head = format!(
        "fan '{}' ({} pwm{}, {tach})",
        f.name, f.pwm_chip, f.pwm_index
    );
    if !f.enabled {
        return format!("{head}: disabled");
    }
    if f.disabled_by_config {
        return format!("{head}: disabled by config, left in BIOS/driver mode");
    }
    let sensors = f
        .sensors
        .iter()
        .map(|s| {
            let mut name = s.chip.clone();
            if let Some(input) = &s.input {
                name.push(' ');
                name.push_str(input);
            }
            if let Some(label) = &s.label {
                let _ = write!(name, " \"{label}\"");
            }
            let lo = s.min_temp_c.unwrap_or(f.min_temp_c);
            let hi = s.max_temp_c.unwrap_or(f.max_temp_c);
            format!("{name} {lo}-{hi}C")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sensors = if sensors.is_empty() {
        "none".to_string()
    } else {
        sensors
    };
    let how = match f.mode {
        FanMode::Fixed => match f.fixed_pwm {
            Some(p) => format!(
                "fixed pwm {p}, pwm {} while any sensor is critical",
                f.max_pwm
            ),
            None => "fixed mode without fixed_pwm".to_string(),
        },
        FanMode::Curve => format!(
            "curve: pwm {} at/below {}, ramp {}..{} up to {}, pwm {} at/above it \
             (a stopped fan is kicked at pwm {})",
            f.min_pwm,
            fmt_c(f.min_temp_c),
            f.min_stop_pwm,
            f.max_pwm,
            fmt_c(f.max_temp_c),
            f.max_pwm,
            f.min_start_pwm,
        ),
    };
    format!("{head}: {how}; sensors (ramp start-full speed): {sensors}")
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
            // Fallback for whatever `thresholds` doesn't cover -- mainly
            // the ACPI thermal zone and the board's IT8625 sensors. Observed
            // live (2026-09-23): CPU package temp normally sits 57-67C
            // under everyday load -- since CPU now has its own
            // datasheet-sourced entry in `thresholds`, that observation no
            // longer directly justifies this fallback, but it's a
            // reasonable generic "getting warm" ceiling for silicon in
            // general absent a specific spec.
            warn_threshold: 75.0,
            critical_threshold: 85.0,
            thresholds: default_temp_thresholds(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct DockerConfig {
    /// Container names to ignore entirely when checking health (e.g. known
    /// noisy/expected-unhealthy containers).
    pub ignore: Vec<String>,
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
    /// `curve` (default) or `fixed` -- see `FanMode`.
    pub mode: FanMode,
    /// The PWM `mode = "fixed"` runs at, within `min_pwm..=max_pwm`.
    /// Required for fixed mode; ignored (with a diagnostic) otherwise.
    pub fixed_pwm: Option<u8>,
    /// How often to re-evaluate this fan's curve and (re)write its pwm.
    /// lm-sensors' fancontrol(8) default INTERVAL is also 1s.
    pub update_secs: u64,
    /// At or below this control temp, the fan is pinned to `min_pwm`.
    pub min_temp_c: f32,
    /// At or above this control temp, the fan is pinned to `max_pwm`.
    pub max_temp_c: f32,
    /// PWM needed to reliably get a *stopped* fan spinning again. Below
    /// this, a stopped fan stays stopped rather than crawl at a PWM too low
    /// to actually start it turning. `lcm-status fan-profile` measures this
    /// empirically per fan rather than guessing.
    pub min_start_pwm: u8,
    /// Once running, the fan is allowed to coast down to this PWM before
    /// it's allowed to stop entirely (also the ramp's value at `min_temp_c`
    /// -- see fan.rs for why that's not `min_pwm`; same as upstream
    /// fancontrol).
    pub min_stop_pwm: u8,
    /// PWM used flat at/below `min_temp_c`. 0 lets the fan stop there; it's
    /// kicked with `min_start_pwm` when the curve next wants it spinning.
    pub min_pwm: u8,
    /// PWM used flat at/above `max_temp_c`. 255 = fully on.
    pub max_pwm: u8,
    /// Which sensors feed this fan's control temp -- the max of all of
    /// them, not just one. Empty means this fan never sees a temp reading,
    /// which effectively disables it (`compute_pwm` has nothing to act on).
    pub sensors: Vec<SensorSelector>,
    /// Log a syslog WARNING if this fan is confirmed running (not
    /// intentionally stopped) but its RPM is below this -- a bearing
    /// wearing out or a partial obstruction can show up as "spinning, but
    /// slower than it should" well before an outright stall. `None`
    /// (default) disables the check; `lcm-status fan-profile`'s measured
    /// max RPM at pwm=255 is a reasonable starting point (e.g. ~60-70% of
    /// it) if you want to set one.
    pub min_expected_rpm: Option<u32>,
    /// Set by `Config::validate` (never read from the file) when this
    /// profile was disabled for being inconsistent, as opposed to
    /// `enabled = false` by choice. fan.rs reports it as "disabled by
    /// config" with a warning health, since the fan is then left in
    /// BIOS/driver mode.
    #[serde(skip)]
    pub disabled_by_config: bool,
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
            mode: FanMode::Curve,
            fixed_pwm: None,
            update_secs: 1,
            min_temp_c: 45.0,
            max_temp_c: 90.0,
            min_start_pwm: 60,
            min_stop_pwm: 55,
            min_pwm: 50,
            max_pwm: 255,
            sensors: Vec::new(),
            min_expected_rpm: None,
            disabled_by_config: false,
        }
    }
}

/// How a fan's PWM is chosen. Either way, everything else in `fan.rs`
/// still applies: manual mode asserted before every write, a stopped or
/// stalled fan kicked with `min_start_pwm`, RPM health checks, and
/// `max_pwm` if every sensor stops reading.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FanMode {
    /// From the temperature curve (`min_temp_c`/`max_temp_c` and friends).
    #[default]
    Curve,
    /// A constant `fixed_pwm`, like ADM's fixed fan mode -- *except* while
    /// any of the fan's sensors is at or above its critical threshold
    /// (`[[temperature.thresholds]]`, else `[temperature]`), which runs it
    /// at `max_pwm` until every sensor is back below its *warning*
    /// threshold. A quiet fixed speed must never ignore a cooking drive,
    /// so fixed mode still needs `sensors`. The curve settings are unused.
    Fixed,
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
#[derive(Debug, Deserialize, Clone, Default)]
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
    /// Override the owning fan's `min_temp_c`/`max_temp_c` for *this*
    /// sensor's own contribution to the curve, instead of sharing the
    /// fan's (falls back to the fan's own value if unset). This matters
    /// because different sensors reach their own danger zone at very
    /// different temperatures: this board's fan curve is CPU-tuned
    /// (`max_temp_c` = 90), but a drive's own critical threshold is 60C --
    /// without its own override, a drive at 60C would only compute to a
    /// modest partial speed against the CPU's curve, not the full-speed
    /// response its own critical threshold warrants. See
    /// `default_fans()`, which sets these to match
    /// `default_temp_thresholds()` for exactly that reason.
    pub min_temp_c: Option<f32>,
    pub max_temp_c: Option<f32>,
}

/// This board's single real fan (AS6704T: `it8625` `pwm1`/`fan1`; fan2/3
/// headers exist on the chip but nothing's physically connected -- see
/// `hal::cpu_and_fan`'s doc comment). Used as `Config`'s default so the
/// daemon needs no `[[fans]]` config at all to behave the way it always
/// has; curve values match the `/etc/fancontrol` config this replaced.
pub fn default_fans() -> Vec<FanProfile> {
    // Curve values are `FanProfile::default()`'s, which are this board's.
    vec![FanProfile {
        name: "chassis".to_string(),
        pwm_chip: "it8625".to_string(),
        fan_index: Some(1),
        sensors: vec![
            // CPU: no override -- shares this fan's own curve (45-90C),
            // the original hand-tuned values.
            SensorSelector {
                chip: "coretemp".to_string(),
                label: Some("Package".to_string()),
                ..Default::default()
            },
            // Drive/NVMe: overridden to their own warn/critical thresholds
            // from `default_temp_thresholds()` (50/60C, 60/70C) rather than
            // sharing the CPU's 45-90C curve -- a drive/SSD at its own
            // critical threshold should already mean max_pwm, not a modest
            // partial speed computed against a curve tuned for a
            // completely different component's danger zone. Keep these in
            // sync with `default_temp_thresholds()` if you change one.
            SensorSelector {
                chip: "drivetemp".to_string(),
                min_resample_secs: Some(30),
                min_temp_c: Some(50.0),
                max_temp_c: Some(60.0),
                ..Default::default()
            },
            SensorSelector {
                chip: "nvme".to_string(),
                min_resample_secs: Some(30),
                min_temp_c: Some(60.0),
                max_temp_c: Some(70.0),
                ..Default::default()
            },
            // The AQC113 10GbE card's PHY/MAC (one hwmon device, named
            // after the interface; the max of the two counts). ADM's own
            // LAN-chip curve starts at 70C and reaches full speed at
            // 100C, which is also this chip's critical threshold.
            SensorSelector {
                chip: "enp9s0".to_string(),
                min_resample_secs: Some(10),
                min_temp_c: Some(70.0),
                max_temp_c: Some(100.0),
                ..Default::default()
            },
        ],
        // `lcm-status fan-profile` measured ~2600 RPM at pwm=255 on this
        // board (2026-09-23) -- well clear of normal operating range
        // (observed ~1300-2000 RPM day to day), so this only fires for a
        // genuinely underperforming fan, not routine low-load speeds.
        min_expected_rpm: Some(500),
        ..FanProfile::default()
    }]
}

impl Config {
    /// Loads `path`, printing any diagnostics to stderr. Never fails: a
    /// missing or unparseable file means defaults, and individual bad
    /// settings fall back as described in `validate`.
    pub fn load(path: &Path) -> Config {
        let (cfg, diagnostics) = Config::load_with_diagnostics(path);
        for d in diagnostics {
            eprintln!("{}: {d}", path.display());
        }
        cfg
    }

    /// `load`, returning the diagnostics instead of printing them (for
    /// `check-config`, which exits non-zero if there are any).
    pub fn load_with_diagnostics(path: &Path) -> (Config, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Config::default(), vec!["not found, using defaults".into()])
            }
            Err(e) => (
                Config::default(),
                vec![format!("failed to read ({e}), using defaults")],
            ),
        }
    }

    /// Parses and validates config text. Unknown keys (typically a typo,
    /// e.g. `enable` for `enabled`) are reported and ignored rather than
    /// rejected: rejecting would throw the whole file away for defaults.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut diagnostics = Vec::new();
        let parsed = serde_ignored::deserialize(toml::Deserializer::new(text), |key| {
            diagnostics.push(format!("unknown setting `{key}`, ignored"));
        });
        match parsed {
            Ok(mut cfg) => {
                diagnostics.extend(Config::validate(&mut cfg));
                (cfg, diagnostics)
            }
            Err(e) => (
                Config::default(),
                vec![format!("failed to parse ({e}), using defaults")],
            ),
        }
    }

    /// Checks settings serde can't, returning one message per problem.
    /// Each problem disables or reverts only the affected piece -- the
    /// rest of the config still applies:
    /// - a bad template falls back to that screen's default;
    /// - an unparseable `[sleep]` time disables night mode;
    /// - a fan profile with an inconsistent curve is disabled, leaving
    ///   that fan in its BIOS/driver mode (which may stop it -- the
    ///   diagnostic and `STATUS` say so) rather than driving it from a
    ///   curve that makes no sense;
    /// - an unparseable `[wol] mode` leaves WOL untouched;
    /// - an `[led]` brightness over 100 is clamped to 100;
    /// - durations over a day and absurd scroll sizes are clamped;
    /// - a NaN or infinite temperature falls back to its default (fan
    ///   curve) or is dropped (threshold override);
    /// - inverted temperature thresholds are reported only;
    /// - a malformed `[[power_schedule]]` entry is disabled; rules that
    ///   are fine alone but clash (see `power::lint`) are reported only.
    fn validate(&mut self) -> Vec<String> {
        let mut errors = self.templates.validate();
        self.clamp_bounds(&mut errors);

        if self.sleep.enabled {
            for (key, value) in [("start", &self.sleep.start), ("end", &self.sleep.end)] {
                if parse_hhmm(value).is_none() {
                    errors.push(format!(
                        "[sleep].{key} = \"{value}\" is not a 24h HH:MM time; night mode disabled"
                    ));
                }
            }
            if parse_hhmm(&self.sleep.start).is_none() || parse_hhmm(&self.sleep.end).is_none() {
                self.sleep.enabled = false;
            }
        }

        if self.cpu_power.enabled {
            let problems = self.cpu_power.problems();
            if !problems.is_empty() {
                errors.push(format!(
                    "[cpu_power]: {}; CPU power limits are left alone",
                    problems.join("; ")
                ));
                self.cpu_power.enabled = false;
            }
        }

        self.sanitize_fan_temps(&mut errors);

        for fan in self.fans.iter_mut().filter(|f| f.enabled) {
            if fan.mode == FanMode::Curve && fan.fixed_pwm.is_some() {
                errors.push(format!(
                    "[[fans]] '{}': fixed_pwm is ignored unless mode = \"fixed\"",
                    fan.name
                ));
            }
            let problems = fan.problems();
            if !problems.is_empty() {
                errors.push(format!(
                    "[[fans]] '{}': {}; fan control disabled for it -- the fan stays in \
                     BIOS/driver automatic mode, which may stop it (on this board's it8625 it \
                     does); fix the config",
                    fan.name,
                    problems.join(", ")
                ));
                fan.enabled = false;
                fan.disabled_by_config = true;
            }
        }

        if !self.wol.nics.is_empty()
            && let Err(e) = crate::wol::parse_mode(&self.wol.mode)
        {
            errors.push(format!(
                "[wol] mode = \"{}\": {e}; Wake-on-LAN left alone",
                self.wol.mode
            ));
            self.wol.nics.clear();
        }

        for (i, rule) in self.power_schedule.iter_mut().enumerate() {
            if rule.enabled
                && let Err(e) = crate::power::Rule::parse(i, rule)
            {
                errors.push(format!(
                    "[[power_schedule]] #{}: {e}; entry disabled",
                    i + 1
                ));
                rule.enabled = false;
            }
        }
        errors.extend(crate::power::lint(&crate::power::parse_rules(
            &self.power_schedule,
        )));

        for (key, value) in [
            ("brightness", &mut self.led.brightness),
            ("night_brightness", &mut self.led.night_brightness),
        ] {
            if let Some(v) = value.as_mut()
                && *v > 100
            {
                errors.push(format!("[led] {key} = {v} is over 100; using 100"));
                *v = 100;
            }
        }

        self.sanitize_thresholds(&mut errors);

        let t = &self.temperature;
        if t.warn_threshold >= t.critical_threshold {
            errors.push(format!(
                "[temperature] warn_threshold ({}) should be below critical_threshold ({})",
                t.warn_threshold, t.critical_threshold
            ));
        }
        for o in &t.thresholds {
            if o.chip.is_empty() {
                errors.push("[[temperature.thresholds]] entry has no `chip`".into());
            } else if o.warn_threshold >= o.critical_threshold {
                errors.push(format!(
                    "[[temperature.thresholds]] '{}': warn_threshold ({}) should be below critical_threshold ({})",
                    o.chip, o.warn_threshold, o.critical_threshold
                ));
            }
        }

        errors
    }
}

/// Range accepted for `[[fans]] update_secs`.
const MIN_FAN_UPDATE_SECS: u64 = 1;
const MAX_FAN_UPDATE_SECS: u64 = 60;

/// Clamps `value` to `max`, noting it in `errors` if that changed it.
fn clamp_to<T: PartialOrd + Copy + std::fmt::Display>(
    errors: &mut Vec<String>,
    key: &str,
    value: &mut T,
    max: T,
) {
    if *value > max {
        errors.push(format!("{key} = {value} is over {max}; using {max}"));
        *value = max;
    }
}

impl Config {
    /// Clamps the settings that size a timer or a buffer, so a typo (or
    /// `9223372036854775807`) can't overflow `Instant` arithmetic or ask
    /// for a gigabyte scroll buffer.
    fn clamp_bounds(&mut self, errors: &mut Vec<String>) {
        let secs = [
            ("[rotation] dwell_secs", &mut self.rotation.dwell_secs),
            (
                "[rotation] resume_after_secs",
                &mut self.rotation.resume_after_secs,
            ),
            (
                "[menu] confirm_timeout_secs",
                &mut self.menu.confirm_timeout_secs,
            ),
            (
                "[refresh] network_min_secs",
                &mut self.refresh.network_min_secs,
            ),
            ("[refresh] pools_min_secs", &mut self.refresh.pools_min_secs),
            ("[refresh] hdd_min_secs", &mut self.refresh.hdd_min_secs),
            (
                "[refresh] temperature_min_secs",
                &mut self.refresh.temperature_min_secs,
            ),
            (
                "[refresh] docker_min_secs",
                &mut self.refresh.docker_min_secs,
            ),
        ];
        for (key, value) in secs {
            clamp_to(errors, key, value, MAX_SECS);
        }
        clamp_to(
            errors,
            "[display] scroll_max_chars",
            &mut self.display.scroll_max_chars,
            MAX_SCROLL_CHARS,
        );
        clamp_to(
            errors,
            "[display] scroll_gap",
            &mut self.display.scroll_gap,
            MAX_SCROLL_GAP,
        );
    }

    /// Replaces NaN/infinite fan-curve temperatures: comparisons with NaN
    /// are all false, so such a curve would pass `FanProfile::problems`
    /// and then compute garbage. The fan's own values revert to the
    /// defaults (checked for consistency afterwards like any other), a
    /// sensor's own override is dropped so it shares the fan's.
    fn sanitize_fan_temps(&mut self, errors: &mut Vec<String>) {
        let defaults = FanProfile::default();
        for fan in &mut self.fans {
            // 0 would spin the control loop and a huge value would freeze
            // the fan after its first update, so keep it in a sane range.
            let clamped = fan
                .update_secs
                .clamp(MIN_FAN_UPDATE_SECS, MAX_FAN_UPDATE_SECS);
            if clamped != fan.update_secs {
                errors.push(format!(
                    "[[fans]] '{}': update_secs = {} is outside {MIN_FAN_UPDATE_SECS}..={MAX_FAN_UPDATE_SECS}; using {clamped}",
                    fan.name, fan.update_secs
                ));
                fan.update_secs = clamped;
            }
            for (key, value, default) in [
                ("min_temp_c", &mut fan.min_temp_c, defaults.min_temp_c),
                ("max_temp_c", &mut fan.max_temp_c, defaults.max_temp_c),
            ] {
                if !value.is_finite() {
                    errors.push(format!(
                        "[[fans]] '{}': {key} = {value} is not a finite number; using {default}",
                        fan.name
                    ));
                    *value = default;
                }
            }
            for sel in &mut fan.sensors {
                for (key, value) in [
                    ("min_temp_c", &mut sel.min_temp_c),
                    ("max_temp_c", &mut sel.max_temp_c),
                ] {
                    if value.is_some_and(|v| !v.is_finite()) {
                        errors.push(format!(
                            "[[fans]] '{}': sensor '{}': {key} is not a finite number; \
                             using the fan's value",
                            fan.name, sel.chip
                        ));
                        *value = None;
                    }
                }
            }
        }
    }

    /// Same for the `[temperature]` thresholds: a NaN would silently never
    /// alert. The global pair reverts to its defaults, an override entry is
    /// dropped (its chip then falls back to the global pair).
    fn sanitize_thresholds(&mut self, errors: &mut Vec<String>) {
        let defaults = TemperatureConfig::default();
        let t = &mut self.temperature;
        for (key, value, default) in [
            (
                "warn_threshold",
                &mut t.warn_threshold,
                defaults.warn_threshold,
            ),
            (
                "critical_threshold",
                &mut t.critical_threshold,
                defaults.critical_threshold,
            ),
        ] {
            if !value.is_finite() {
                errors.push(format!(
                    "[temperature] {key} = {value} is not a finite number; using {default}"
                ));
                *value = default;
            }
        }
        t.thresholds.retain(|o| {
            let ok = o.warn_threshold.is_finite() && o.critical_threshold.is_finite();
            if !ok {
                errors.push(format!(
                    "[[temperature.thresholds]] '{}': thresholds must be finite numbers; \
                     entry ignored",
                    o.chip
                ));
            }
            ok
        });
    }
}

impl FanProfile {
    /// Inconsistencies that make this profile's curve meaningless.
    fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.pwm_chip.is_empty() {
            out.push("no pwm_chip".to_string());
        }
        if self.min_temp_c >= self.max_temp_c {
            out.push(format!(
                "min_temp_c ({}) must be below max_temp_c ({})",
                self.min_temp_c, self.max_temp_c
            ));
        }
        if self.min_pwm > self.max_pwm || self.min_stop_pwm > self.max_pwm {
            out.push(format!(
                "min_pwm ({}) and min_stop_pwm ({}) must not exceed max_pwm ({})",
                self.min_pwm, self.min_stop_pwm, self.max_pwm
            ));
        }
        if self.mode == FanMode::Fixed {
            match self.fixed_pwm {
                None => out.push("mode = \"fixed\" needs fixed_pwm".to_string()),
                Some(pwm) if pwm < self.min_pwm || pwm > self.max_pwm => out.push(format!(
                    "fixed_pwm ({pwm}) must be within min_pwm..=max_pwm ({}..={})",
                    self.min_pwm, self.max_pwm
                )),
                Some(_) => {}
            }
            if self.sensors.is_empty() {
                out.push(
                    "mode = \"fixed\" needs sensors, for its critical-temperature override"
                        .to_string(),
                );
            }
        }
        for sel in &self.sensors {
            if sel.chip.is_empty() {
                out.push("a sensor with no chip".to_string());
            }
            let min_t = sel.min_temp_c.unwrap_or(self.min_temp_c);
            let max_t = sel.max_temp_c.unwrap_or(self.max_temp_c);
            if min_t >= max_t {
                out.push(format!(
                    "sensor '{}': min_temp_c ({min_t}) must be below max_temp_c ({max_t})",
                    sel.chip
                ));
            }
        }
        out
    }
}

impl SleepConfig {
    /// Whether local time `now` (hour, minute) falls in the sleep window.
    /// `end` is exclusive, and a window whose end is before its start
    /// wraps past midnight. False if either bound doesn't parse (`validate`
    /// has already reported and disabled that case).
    pub fn contains(&self, now: (u32, u32)) -> bool {
        let (Some(start), Some(end)) = (parse_hhmm(&self.start), parse_hhmm(&self.end)) else {
            return false;
        };
        let now = now.0 * 60 + now.1;
        if start <= end {
            (start..end).contains(&now)
        } else {
            now >= start || now < end
        }
    }
}

/// "HH:MM" (24h) as minutes since midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Config, Vec<String>) {
        Config::parse(text)
    }

    #[test]
    fn startup_thresholds_list_overrides_and_label_the_fallback() {
        let (cfg, errors) = parse(
            "[temperature]\nwarn_threshold = 75.0\ncritical_threshold = 85.0\n\
             [[temperature.thresholds]]\nchip = \"coretemp\"\nwarn_threshold = 90.0\n\
             critical_threshold = 100.0\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            describe_thresholds(&cfg.temperature),
            "coretemp 90C/100C, other chips 75C/85C"
        );
    }

    #[test]
    fn startup_sensors_show_the_thresholds_each_one_is_judged_against() {
        let (cfg, _) = parse(
            "[[temperature.thresholds]]\nchip = \"coretemp\"\nwarn_threshold = 90.0\n\
             critical_threshold = 100.0\n",
        );
        let temps = vec![
            (
                "coretemp".to_string(),
                "coretemp temp1 \"Package id 0\"".to_string(),
                58.4,
            ),
            ("it8625".to_string(), "it8625 temp2".to_string(), 41.0),
        ];
        assert_eq!(
            describe_sensors(&cfg.temperature, &temps),
            "sensors: coretemp temp1 \"Package id 0\" 58C (warn 90C, crit 100C); \
             it8625 temp2 41C (warn 75C, crit 85C)"
        );
        assert_eq!(
            describe_sensors(&cfg.temperature, &[]),
            "sensors: none detected"
        );
    }

    #[test]
    fn startup_fan_line_gives_the_curve_and_each_sensors_own_range() {
        let fan = &default_fans()[0];
        assert_eq!(
            describe_fan(fan),
            "fan 'chassis' (it8625 pwm1, tach fan1): curve: pwm 50 at/below 45C, \
             ramp 55..255 up to 90C, pwm 255 at/above it (a stopped fan is kicked at pwm 60); \
             sensors (ramp start-full speed): coretemp \"Package\" 45-90C, \
             drivetemp 50-60C, nvme 60-70C, enp9s0 70-100C"
        );
    }

    #[test]
    fn startup_fan_line_covers_fixed_and_disabled_fans() {
        let mut fan = default_fans()[0].clone();
        fan.mode = FanMode::Fixed;
        fan.fixed_pwm = Some(120);
        assert!(
            describe_fan(&fan).contains("fixed pwm 120, pwm 255 while any sensor is critical"),
            "{}",
            describe_fan(&fan)
        );
        fan.enabled = false;
        assert_eq!(
            describe_fan(&fan),
            "fan 'chassis' (it8625 pwm1, tach fan1): disabled"
        );
    }

    #[test]
    fn huge_durations_and_sizes_are_clamped() {
        let (cfg, errors) = parse(
            "[rotation]\ndwell_secs = 9223372036854775807\nresume_after_secs = 9223372036854775807\n\
             [menu]\nconfirm_timeout_secs = 9223372036854775807\n\
             [refresh]\nnetwork_min_secs = 86401\npools_min_secs = 9223372036854775807\n\
             hdd_min_secs = 9223372036854775807\ntemperature_min_secs = 9223372036854775807\n\
             docker_min_secs = 9223372036854775807\n\
             [display]\nscroll_max_chars = 9223372036854775807\nscroll_gap = 100000\n",
        );
        assert_eq!(errors.len(), 10, "{errors:?}");
        assert_eq!(cfg.rotation.dwell_secs, MAX_SECS);
        assert_eq!(cfg.rotation.resume_after_secs, MAX_SECS);
        assert_eq!(cfg.menu.confirm_timeout_secs, MAX_SECS);
        assert_eq!(cfg.refresh.network_min_secs, MAX_SECS);
        assert_eq!(cfg.refresh.docker_min_secs, MAX_SECS);
        assert_eq!(cfg.display.scroll_max_chars, MAX_SCROLL_CHARS);
        assert_eq!(cfg.display.scroll_gap, MAX_SCROLL_GAP);
    }

    #[test]
    fn cpu_power_is_off_unless_enabled_and_parses_whole_units() {
        let (cfg, errors) = Config::parse("");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!cfg.cpu_power.enabled);
        let (cfg, errors) =
            Config::parse("[cpu_power]\nenabled = true\npl1_w = 12\npl2_w = 14\ntau_secs = 120\n");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(cfg.cpu_power.enabled);
        assert_eq!(
            (cfg.cpu_power.pl1_w, cfg.cpu_power.stock_pl2_w),
            (12, 25),
            "stock defaults to this board's firmware values"
        );
    }

    #[test]
    fn bad_cpu_power_settings_disable_it_rather_than_guess() {
        for bad in [
            "pl1_w = 0",
            "pl1_w = 20\npl2_w = 14",
            "pl2_w = 100000",
            "tau_secs = 0",
            "stock_pl1_w = 30",
            "check_secs = 0",
            "check_secs = 61",
        ] {
            let (cfg, errors) = Config::parse(&format!("[cpu_power]\nenabled = true\n{bad}\n"));
            assert!(!cfg.cpu_power.enabled, "{bad}");
            assert!(
                errors.iter().any(|e| e.contains("[cpu_power]")),
                "{bad}: {errors:?}"
            );
        }
        // A problem in a section that is off isn't reported.
        let (_, errors) = Config::parse("[cpu_power]\npl1_w = 0\n");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn fan_update_secs_is_clamped_to_one_to_sixty() {
        for (given, want) in [
            (0u64, 1u64),
            (1, 1),
            (60, 60),
            (61, 60),
            (9_223_372_036_854_775_807, 60),
        ] {
            let (cfg, errors) = parse(&format!(
                "[[fans]]\nname = \"f\"\npwm_chip = \"it8625\"\nupdate_secs = {given}\n"
            ));
            let fan = cfg.fans.iter().find(|f| f.name == "f").unwrap();
            assert_eq!(fan.update_secs, want, "{given}");
            let flagged = errors.iter().any(|e| e.contains("update_secs"));
            assert_eq!(flagged, given != want, "{given}: {errors:?}");
        }
    }

    #[test]
    fn values_at_the_bound_and_defaults_are_left_alone() {
        let (cfg, errors) = parse("[rotation]\ndwell_secs = 86400\n");
        assert_eq!(errors, Vec::<String>::new());
        assert_eq!(cfg.rotation.dwell_secs, 86_400);
        assert_eq!(cfg.rotation.resume_after_secs, 30);
    }

    #[test]
    fn non_finite_thresholds_are_replaced_with_a_diagnostic() {
        let (cfg, errors) = parse(
            "[temperature]\nwarn_threshold = nan\ncritical_threshold = inf\n\
             [[temperature.thresholds]]\nchip = \"a\"\nwarn_threshold = nan\ncritical_threshold = 90.0\n\
             [[temperature.thresholds]]\nchip = \"b\"\nwarn_threshold = 50.0\ncritical_threshold = -inf\n\
             [[temperature.thresholds]]\nchip = \"c\"\nwarn_threshold = 50.0\ncritical_threshold = 60.0\n",
        );
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!((cfg.temperature.warn_threshold - 75.0).abs() < f32::EPSILON);
        assert!((cfg.temperature.critical_threshold - 85.0).abs() < f32::EPSILON);
        let chips: Vec<&str> = cfg
            .temperature
            .thresholds
            .iter()
            .map(|o| o.chip.as_str())
            .collect();
        assert_eq!(chips, ["c"]);
    }

    #[test]
    fn non_finite_fan_temps_revert_to_usable_values() {
        let (cfg, errors) = parse(
            "[[fans]]\nname = \"f\"\npwm_chip = \"it8625\"\nmin_temp_c = nan\nmax_temp_c = inf\n\
             [[fans.sensors]]\nchip = \"coretemp\"\nmin_temp_c = nan\nmax_temp_c = 80.0\n",
        );
        assert_eq!(errors.len(), 3, "{errors:?}");
        let fan = &cfg.fans[0];
        assert!(fan.enabled, "a repaired curve keeps the fan under control");
        assert!(fan.min_temp_c.is_finite() && fan.max_temp_c.is_finite());
        assert_eq!(fan.sensors[0].min_temp_c, None);
        assert_eq!(fan.sensors[0].max_temp_c, Some(80.0));
    }

    #[test]
    fn default_templates_are_valid() {
        assert_eq!(TemplatesConfig::default().validate(), Vec::<String>::new());
    }

    #[test]
    fn omitted_line_is_blank() {
        let (cfg, errors) = parse("[templates.pool]\nline1 = \"{free} free\"\n");
        assert_eq!(errors, Vec::<String>::new());
        assert_eq!(cfg.templates.pool.line0, "");
        assert_eq!(cfg.templates.pool.line1, "{free} free");
        // Blocks not mentioned at all keep their defaults.
        assert_eq!(cfg.templates.fan.line1, "{rpm} RPM");
    }

    #[test]
    fn unknown_variable_falls_back_per_screen() {
        let (cfg, errors) = parse(
            "[templates.pool]\nline1 = \"{fre} free\"\n[templates.fan]\nline0 = \"CHASSIS\"\n",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("{fre}"));
        assert_eq!(cfg.templates.pool.line1, "{alloc}/{size} {cap}");
        assert_eq!(cfg.templates.fan.line0, "CHASSIS");
    }

    #[test]
    fn partial_order_appends_the_rest() {
        let (cfg, _) = parse("[screens]\norder = [\"docker\", \"hdd\", \"docker\"]\n");
        assert_eq!(
            cfg.screens.effective_order(),
            vec![
                Category::Docker,
                Category::Hdd,
                Category::Network,
                Category::Pools,
                Category::Temperature
            ]
        );
    }

    #[test]
    fn example_config_parses_cleanly() {
        let (cfg, diagnostics) = parse(include_str!("../lcm-status.example.toml"));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(cfg.power_schedule.is_empty(), "must ship commented out");
    }

    #[test]
    fn example_power_schedule_parses_cleanly_once_uncommented() {
        // Commented-out settings are the lines with no space after `#`.
        let uncommented: String = include_str!("../lcm-status.example.toml")
            .lines()
            .map(|l| match l.strip_prefix('#') {
                Some(rest) if rest.starts_with(|c: char| c.is_ascii_lowercase() || c == '[') => {
                    rest
                }
                _ => l,
            })
            .flat_map(|l| [l, "\n"])
            .collect();
        let (cfg, diagnostics) = parse(&uncommented);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(cfg.power_schedule.len(), 3);
        assert_eq!(crate::power::parse_rules(&cfg.power_schedule).len(), 3);
    }

    #[test]
    fn unknown_keys_are_reported_but_the_rest_still_applies() {
        let (cfg, diagnostics) = parse("[sleep]\nenable = true\nstart = \"21:00\"\n");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("sleep.enable"), "{diagnostics:?}");
        assert_eq!(cfg.sleep.start, "21:00");
    }

    #[test]
    fn bad_sleep_time_disables_night_mode() {
        let (cfg, diagnostics) = parse("[sleep]\nenabled = true\nstart = \"25:00\"\n");
        assert!(!cfg.sleep.enabled);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn inconsistent_fan_curve_disables_only_that_fan() {
        let (cfg, diagnostics) = parse(
            "[[fans]]\nname = \"bad\"\npwm_chip = \"it8625\"\nmin_temp_c = 90.0\nmax_temp_c = 45.0\n\
             [[fans]]\nname = \"good\"\npwm_chip = \"it8625\"\npwm_index = 2\n",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(!cfg.fans[0].enabled);
        assert!(cfg.fans[1].enabled);
    }

    #[test]
    fn led_settings_parse_and_brightness_is_clamped() {
        let (cfg, diagnostics) = parse(
            "[led]\nbrightness = 150\nnight_brightness = 5\nbay_mode = \"activity\"\n\
             [sleep]\nlcd_off = false\n",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("brightness"));
        assert_eq!(cfg.led.brightness, Some(100));
        assert_eq!(cfg.led.night_brightness, Some(5));
        assert_eq!(cfg.led.bay_mode, Some(crate::led::BayLedMode::Activity));
        assert!(!cfg.sleep.lcd_off);
        assert!(Config::default().sleep.lcd_off);
    }

    #[test]
    fn bad_wol_mode_leaves_wol_alone() {
        let (cfg, diagnostics) = parse("[wol]\nnics = [\"enp2s0\"]\nmode = \"gs\"\n");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(cfg.wol.nics, Vec::<String>::new());
        let (cfg, diagnostics) = parse("[wol]\nnics = [\"enp2s0\"]\n");
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(cfg.wol.mode, "g");
    }

    #[test]
    fn fixed_fan_mode_is_validated() {
        let fan = |extra: &str| {
            parse(&format!(
                "[[fans]]\npwm_chip = \"it8625\"\n{extra}\n[[fans.sensors]]\nchip = \"drivetemp\"\n"
            ))
        };
        let (cfg, diagnostics) = fan("mode = \"fixed\"\nfixed_pwm = 120");
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(cfg.fans[0].mode, FanMode::Fixed);
        assert!(cfg.fans[0].enabled);

        for bad in [
            "mode = \"fixed\"",                 // no fixed_pwm
            "mode = \"fixed\"\nfixed_pwm = 20", // below min_pwm (50)
            "mode = \"fixed\"\nfixed_pwm = 200\nmax_pwm = 180",
        ] {
            let (cfg, diagnostics) = fan(bad);
            assert_eq!(diagnostics.len(), 1, "{bad}: {diagnostics:?}");
            assert!(!cfg.fans[0].enabled, "{bad}");
        }

        // No sensors: nothing could trigger the critical override.
        let (cfg, _) =
            parse("[[fans]]\npwm_chip = \"it8625\"\nmode = \"fixed\"\nfixed_pwm = 120\n");
        assert!(!cfg.fans[0].enabled);

        // fixed_pwm without fixed mode: reported, but the fan still runs.
        let (cfg, diagnostics) = fan("fixed_pwm = 120");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(cfg.fans[0].enabled);
    }

    #[test]
    fn bad_power_schedule_entry_is_disabled_not_fatal() {
        let (cfg, diagnostics) = parse(
            "[[power_schedule]]\ndays = \"weekdays\"\ntime = \"07:30\"\naction = \"power_on\"\n\
             [[power_schedule]]\ndays = [\"mon\", \"funday\"]\ntime = \"23:00\"\naction = \"shutdown\"\n\
             [sleep]\nstart = \"21:00\"\n",
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("#2") && diagnostics[0].contains("funday"));
        assert!(cfg.power_schedule[0].enabled);
        assert_eq!(cfg.power_schedule[0].days, ["weekdays"]);
        assert!(!cfg.power_schedule[1].enabled);
        // The rest of the file still applies.
        assert_eq!(cfg.sleep.start, "21:00");
    }

    #[test]
    fn the_default_socket_group_is_a_builtin_truenas_group() {
        assert_eq!(Config::default().socket.group, "builtin_administrators");
        let (cfg, diagnostics) = parse("");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(cfg.socket.group, "builtin_administrators");
    }

    #[test]
    fn a_config_naming_the_old_socket_group_still_parses() {
        let (cfg, diagnostics) = parse("[socket]\ngroup = \"lcm-status\"\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(cfg.socket.group, "lcm-status");
        assert_eq!(cfg.socket.path, "/run/lcm-status.sock");
    }

    #[test]
    fn example_config_uses_the_default_socket_group() {
        let (cfg, _) = parse(include_str!("../lcm-status.example.toml"));
        assert_eq!(cfg.socket.group, DEFAULT_SOCKET_GROUP);
    }

    #[test]
    fn default_config_is_valid() {
        assert_eq!(Config::default().validate(), Vec::<String>::new());
    }

    #[test]
    fn sleep_window() {
        let window = |start: &str, end: &str| SleepConfig {
            enabled: true,
            start: start.into(),
            end: end.into(),
            lcd_off: true,
        };
        let night = window("22:00", "06:00");
        assert!(night.contains((23, 30)));
        assert!(night.contains((0, 0)));
        assert!(night.contains((5, 59)));
        assert!(!night.contains((6, 0)));
        assert!(!night.contains((21, 59)));
        assert!(night.contains((22, 0)));

        let day = window("09:00", "17:00");
        assert!(day.contains((12, 0)));
        assert!(!day.contains((17, 0)));
        assert!(!day.contains((8, 59)));

        assert!(!window("22:00", "nope").contains((23, 0)));
        assert!(!window("10:00", "10:00").contains((10, 0)));
    }

    #[test]
    fn the_aqc113_has_thresholds_and_a_fan_sensor() {
        let cfg = Config::default();
        assert_eq!(
            resolve_temp_threshold(&cfg.temperature, "enp9s0"),
            (80.0, 100.0)
        );
        let sensor = cfg.fans[0].sensors.iter().find(|s| s.chip == "enp9s0");
        let sensor = sensor.expect("AQC113 sensor on the default fan");
        // Full speed exactly at its critical threshold.
        assert_eq!(sensor.max_temp_c, Some(100.0));
    }
}
