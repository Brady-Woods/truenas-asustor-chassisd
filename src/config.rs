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
    pub network: NetworkConfig,
    pub templates: TemplatesConfig,
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
            network: NetworkConfig::default(),
            templates: TemplatesConfig::default(),
            fans: default_fans(),
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
}

impl Default for SleepConfig {
    fn default() -> Self {
        SleepConfig {
            enabled: false,
            start: "23:00".to_string(),
            end: "07:00".to_string(),
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
        LedConfig {
            nic_mode: crate::led::NicLedMode::Link,
        }
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
/// **Not included, deliberately:** the AQC113 NIC's board-level PHY/MAC
/// temperature sensors. No public datasheet with a numeric junction/case
/// limit was found for this chip (Marvell's technical datasheets aren't
/// publicly indexed the way Intel's/WD's are) -- rather than fabricate a
/// specific-looking number with no real source, this chip falls back to
/// the global `warn_threshold`/`critical_threshold` default. Worth
/// revisiting if Marvell's actual datasheet ever turns up.
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
        .map(|t| (t.warn_threshold, t.critical_threshold))
        .unwrap_or((cfg.warn_threshold, cfg.critical_threshold))
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
            // the ACPI thermal zone and the AQC113 NIC's sensors on this
            // board (see default_temp_thresholds() for why the NIC has no
            // dedicated, datasheet-sourced entry of its own). Observed
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
    /// PWM used flat at/below min_temp_c. 0 lets the fan stop there; it's
    /// kicked with `min_start_pwm` when the curve next wants it spinning.
    pub min_pwm: u8,
    /// PWM used flat at/above max_temp_c. 255 = fully on.
    pub max_pwm: u8,
    /// Which sensors feed this fan's control temp -- the max of all of
    /// them, not just one. Empty means this fan never sees a temp reading,
    /// which effectively disables it (compute_pwm has nothing to act on).
    pub sensors: Vec<SensorSelector>,
    /// Log a syslog WARNING if this fan is confirmed running (not
    /// intentionally stopped) but its RPM is below this -- a bearing
    /// wearing out or a partial obstruction can show up as "spinning, but
    /// slower than it should" well before an outright stall. `None`
    /// (default) disables the check; `lcm-status fan-profile`'s measured
    /// max RPM at pwm=255 is a reasonable starting point (e.g. ~60-70% of
    /// it) if you want to set one.
    pub min_expected_rpm: Option<u32>,
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
            min_expected_rpm: None,
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
    /// (max_temp_c=90), but a drive's own critical threshold is 60C --
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
    ///   that fan in its BIOS/driver mode rather than driving it from a
    ///   curve that makes no sense;
    /// - inverted temperature thresholds are reported only.
    fn validate(&mut self) -> Vec<String> {
        let mut errors = self.templates.validate();

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

        for fan in self.fans.iter_mut().filter(|f| f.enabled) {
            let problems = fan.problems();
            if !problems.is_empty() {
                errors.push(format!(
                    "[[fans]] '{}': {}; fan control disabled for it",
                    fan.name,
                    problems.join(", ")
                ));
                fan.enabled = false;
            }
        }

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
fn parse_hhmm(s: &str) -> Option<u32> {
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
    fn default_templates_are_valid() {
        assert!(TemplatesConfig::default().validate().is_empty());
    }

    #[test]
    fn omitted_line_is_blank() {
        let (cfg, errors) = parse("[templates.pool]\nline1 = \"{free} free\"\n");
        assert!(errors.is_empty());
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
        let (_, diagnostics) = parse(include_str!("../lcm-status.example.toml"));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
    fn default_config_is_valid() {
        assert!(Config::default().validate().is_empty());
    }

    #[test]
    fn sleep_window() {
        let window = |start: &str, end: &str| SleepConfig {
            enabled: true,
            start: start.into(),
            end: end.into(),
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
}
