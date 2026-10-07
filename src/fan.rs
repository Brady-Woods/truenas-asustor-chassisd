//! Fan control -- drives one pwm output per configured `FanProfile`
//! (`config.rs`) from whichever of its selected sensors reads hottest.
//!
//! This replaces lm-sensors' `fancontrol` package, which used to be relied
//! on by hand (installed once, outside any of our own deploy tooling,
//! before this project existed) rather than deployed update-safely. A
//! TrueNAS SCALE update landing on a fresh boot environment exposed that:
//! it wasn't just `/etc/fancontrol` (the config) that failed to carry
//! forward, the `fancontrol` binary itself genuinely isn't part of the
//! base image. Reimplementing the (small) amount of curve logic here means
//! fan control depends on nothing but this daemon, which we already build
//! and deploy update-safely for the LED/LCD work.
//!
//! The curve algorithm is ported from the actual upstream `fancontrol(8)`
//! shell script (`UpdateFanSpeeds`, lm-sensors' `fancontrol` package),
//! read directly rather than reimplemented from memory -- it has one
//! non-obvious property worth calling out: the linear ramp between
//! `min_temp_c` and `max_temp_c` interpolates from `min_stop_pwm` (not
//! `min_pwm`) up to `max_pwm`. `min_pwm` is only used flat, below
//! `min_temp_c`. See the ASCII graph in lm-sensors' fancontrol.txt if this
//! is confusing on its own -- it was for us too, the first time around.
//!
//! Three ways this goes further than upstream fancontrol:
//! - **Multiple sensors per fan, not one.** `FanProfile::sensors` is a
//!   list of whichever currently read as connected (see
//!   `hal::read_temp_input`), so e.g. a hot drive ramps the fan even with
//!   the CPU idle.
//! - **Each sensor can have its own curve endpoints**
//!   (`SensorSelector::min_temp_c`/`max_temp_c`, falling back to the fan
//!   profile's own), not just its own reading fed through one shared
//!   curve -- see `target_pwm_from_sensors`. A drive's own critical
//!   threshold (60C) is nowhere near a CPU-tuned curve's `max_temp_c`
//!   (90C); without its own endpoints, a drive at 60C would only compute
//!   to a modest partial speed, not the full-speed response its own
//!   danger zone warrants. Each sensor is evaluated against its own curve
//!   independently, and the *worst resulting PWM* wins -- not the worst
//!   raw temperature fed through a single curve.
//! - **Multiple fans, not one.** `Config::fans` is a `Vec`; `main.rs` runs
//!   one `FanController` per entry, independently, on a dedicated thread
//!   (`FanService`) so the main loop can never stall fan control.
//!
//! Sensors are read off the control path: each selector has its own
//! reader thread (`SensorFeed`) that samples sysfs on its own schedule and
//! publishes the latest value. A `drivetemp` read can block indefinitely
//! on a wedged drive; with reads inline that would freeze the fan thread --
//! and every other fan's curve with it. Now a wedged read only makes that
//! one selector's sample go stale, which counts as "lost" (below).
//!
//! A selector that *used to* read and now doesn't is never ignored: after
//! a short grace the fan is held at `max_pwm` (a hot drive must not be
//! forgotten just because its sensor vanished), and released only after
//! the selector has read steadily for a while, so a flapping sensor can't
//! make the fan oscillate.
//!
//! A fan can instead run at a constant PWM (`mode = "fixed"`, like ADM's
//! fixed fan mode -- `config::FanMode`). Only the *target* changes: it
//! goes through the same kick/stall/RPM-health handling, still holds
//! `max_pwm` if every sensor stops reading, and still goes to `max_pwm`
//! while any of its sensors is at a critical temperature (see
//! `update_critical_override`).
//!
//! That critical override applies in both modes: whatever the curve (or
//! the fixed speed) says, a sensor at or above its critical threshold
//! pegs the fan at `max_pwm` until everything is back below warning. A
//! curve's own endpoints may be tuned to hit `max_pwm` there anyway, but
//! the guarantee shouldn't depend on them being kept in sync.
//!
//! `lcm-status fan-profile` (`fan_calibrate.rs`) is the discovery
//! counterpart to `pwmconfig`: it finds which pwm outputs and sensors
//! actually exist and what a fan's real min-start/min-stop PWM is, rather
//! than this module guessing.

use crate::config::{
    FanMode, FanProfile, SensorSelector, TemperatureConfig, resolve_temp_threshold,
};
use crate::hal::{glob_hwmon_in, is_over_range, read_sysfs_raw_f32, resolve_selector_in};
use crate::socket::Level;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How often the fan thread wakes to give each controller a chance to
/// tick. Each controller still only acts every `update_secs`.
const FAN_THREAD_INTERVAL: Duration = Duration::from_millis(100);

/// Shortest and longest a selector's reader thread waits between samples.
/// The ceiling bounds how stale a temperature can be even if a config
/// asks for hours between reads.
const MIN_SENSOR_INTERVAL_SECS: u64 = 1;
const MAX_SENSOR_INTERVAL_SECS: u64 = 300;
/// A sample older than three sampling intervals (but at least this long)
/// is stale: the reader is wedged or gone, and the selector counts as
/// having no reading.
const MIN_SENSOR_MAX_AGE: Duration = Duration::from_secs(10);

/// How long the fan controller tolerates each failure before reacting.
/// A field of `FanController` so tests can shrink them.
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// A selector that used to read has been silent this long: hold
    /// `max_pwm` for the fan.
    sensor_loss_grace: Duration,
    /// ...and it must then read continuously this long before the hold is
    /// released (anti-flap).
    sensor_recovery_hold: Duration,
    /// A fan this daemon can't write (chip not resolved, writes failing)
    /// is a WARNING after this long, so a normal boot -- the platform
    /// driver loads after this unit starts -- doesn't raise one...
    control_warn_after: Duration,
    /// ...and CRITICAL after this long.
    control_critical_after: Duration,
    /// With the chip resolved but no sensor having *ever* read, the fan is
    /// taken over at `max_pwm` after this long (rather than left in
    /// BIOS/driver automatic mode, which can stop it).
    sensor_takeover_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            sensor_loss_grace: Duration::from_secs(10),
            sensor_recovery_hold: Duration::from_secs(30),
            control_warn_after: Duration::from_secs(30),
            control_critical_after: Duration::from_secs(180),
            sensor_takeover_after: Duration::from_secs(30),
        }
    }
}

/// A configured, enabled fan this daemon is not currently able to drive.
#[derive(Debug)]
struct ControlLoss {
    since: Instant,
    /// Latest reason, for the log line.
    why: String,
    warned: bool,
    critical: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One published sensor sample: when it was taken and the hottest value
/// the selector produced (`None`: it matched no readable sensor).
#[derive(Debug, Default, Clone, Copy)]
struct Sample {
    at: Option<Instant>,
    value: Option<f32>,
}

/// The fan controller's view of one `SensorSelector`: the latest sample
/// (written by the selector's reader thread, if one is running) plus the
/// loss/recovery tracking the control thread keeps about it.
#[derive(Debug)]
struct SensorFeed {
    shared: Arc<Mutex<Sample>>,
    /// Older than this, the sample is stale and the selector has no reading.
    max_age: Duration,
    /// Has this selector ever produced a reading? Only a selector that has
    /// can be "lost"; one that never matched anything (no NVMe installed)
    /// is just absent.
    ever_read: bool,
    /// Not reading since (while `ever_read`), and not yet `held`.
    lost_since: Option<Instant>,
    /// Past the grace: the fan is held at `max_pwm` for this selector.
    held: bool,
    /// While `held`: reading steadily since this moment.
    back_since: Option<Instant>,
}

impl SensorFeed {
    fn new(sel: &SensorSelector, update_secs: u64) -> Self {
        let secs = sampling_secs(sel, update_secs);
        SensorFeed {
            shared: Arc::new(Mutex::new(Sample::default())),
            max_age: Duration::from_secs(secs.saturating_mul(3)).max(MIN_SENSOR_MAX_AGE),
            ever_read: false,
            lost_since: None,
            held: false,
            back_since: None,
        }
    }

    /// The selector's reading now: its latest sample's value, unless that
    /// sample is missing or stale.
    fn current(&self) -> Option<f32> {
        let sample = *lock(&self.shared);
        sample
            .at
            .filter(|at| at.elapsed() <= self.max_age)
            .and(sample.value)
    }
}

/// Seconds between samples of `sel`: its own `min_resample_secs`, else the
/// fan's `update_secs`, clamped to the supported range.
fn sampling_secs(sel: &SensorSelector, update_secs: u64) -> u64 {
    sel.min_resample_secs
        .unwrap_or(update_secs)
        .clamp(MIN_SENSOR_INTERVAL_SECS, MAX_SENSOR_INTERVAL_SECS)
}

/// Runs `read` on a new thread every `interval` until `stop`, publishing
/// each result into `shared`. If `read` blocks forever the thread simply
/// stays blocked (it is never joined) and the sample goes stale -- that is
/// the point: nothing on the control path ever waits for it.
fn spawn_reader(
    name: String,
    interval: Duration,
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<Sample>>,
    mut read: impl FnMut() -> Option<f32> + Send + 'static,
) -> std::io::Result<()> {
    thread::Builder::new().name(name).spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let value = read();
            *lock(&shared) = Sample {
                at: Some(Instant::now()),
                value,
            };
            let slept = Instant::now();
            while slept.elapsed() < interval && !stop.load(Ordering::Relaxed) {
                thread::sleep(FAN_THREAD_INTERVAL);
            }
        }
    })?;
    Ok(())
}

/// What the main loop needs from the fan thread: the worst current health
/// (for the status LED) and one status line per fan (for `STATUS`).
#[derive(Debug, Clone)]
pub struct FanStatus {
    pub health: Level,
    pub lines: Vec<String>,
}

/// Fan control, running on its own thread so nothing the main loop does
/// (a slow `smartctl`, a hung `zpool`, a stuck socket client) can stall
/// it. The controllers are owned by that thread; the main loop only sees
/// the published `FanStatus`.
pub struct FanService {
    status: Arc<Mutex<FanStatus>>,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl FanService {
    /// Starts one `FanController` per profile on a dedicated thread. Each
    /// ticks immediately (no `last_tick` yet), so curves apply from startup.
    /// `temperature` supplies the critical/warning thresholds fixed-mode
    /// fans override on.
    pub fn spawn(
        profiles: Vec<FanProfile>,
        temperature: &TemperatureConfig,
    ) -> std::io::Result<Self> {
        let status = Arc::new(Mutex::new(FanStatus {
            health: Level::Info,
            lines: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let temperature = temperature.clone();
        let handle = thread::Builder::new().name("fans".into()).spawn({
            let status = Arc::clone(&status);
            let stop = Arc::clone(&stop);
            move || {
                let mut fans: Vec<FanController> = profiles
                    .into_iter()
                    .map(|p| FanController::new(p, &temperature))
                    .collect();
                for f in &mut fans {
                    f.start_readers(&stop);
                }
                while !stop.load(Ordering::Relaxed) {
                    for f in &mut fans {
                        f.tick();
                    }
                    let snapshot = FanStatus {
                        health: worst_health(&fans),
                        lines: fans.iter().map(FanController::status_line).collect(),
                    };
                    *lock(&status) = snapshot;
                    thread::sleep(FAN_THREAD_INTERVAL);
                }
            }
        })?;
        Ok(FanService {
            status,
            stop,
            handle,
        })
    }

    pub fn status(&self) -> FanStatus {
        lock(&self.status).clone()
    }

    /// True if the fan thread has exited on its own, i.e. it panicked.
    pub fn has_died(&self) -> bool {
        self.handle.is_finished()
    }

    /// Stops the fan thread and waits for it to exit.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn worst_health(fans: &[FanController]) -> Level {
    fans.iter()
        .map(FanController::health_level)
        .max()
        .unwrap_or(Level::Info)
}

/// Consecutive failed restart attempts (each ~1-2s apart, gated by
/// `kick_until` + `update_secs`) before a stalled fan is logged as
/// CRITICAL rather than just WARNING -- long enough to not fire on one
/// transient blip that self-heals next tick, short enough to still be a
/// prompt alert (a few seconds, not minutes).
const UNRESPONSIVE_AFTER_STALLS: u32 = 3;

/// What `FanController::plan` wants written this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PwmCommand {
    /// Write this value, as the steady-state curve output.
    Set(u8),
    /// Write `min_start_pwm` and hold it for a second to get a stopped or
    /// stalled fan turning, before resuming the curve.
    Kick(u8),
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent health/override flags, each with its own log transitions"
)]
pub struct FanController {
    profile: FanProfile,
    hwmon: Option<String>,
    last_pwm: Option<u8>,
    /// Set while a stalled/stopped fan is being kicked with `min_start_pwm`;
    /// the real curve value is written once this elapses. Non-blocking by
    /// design -- upstream fancontrol just `sleep 1`s inline, which is fine
    /// for a single-purpose daemon but would freeze LCD/button handling
    /// here for a full second. Spread across normal tick cadence instead.
    kick_until: Option<Instant>,
    /// Parallel to `profile.sensors`: each selector's latest sample and
    /// loss tracking (see `SensorFeed`).
    feeds: Vec<SensorFeed>,
    /// The hwmon class directory; `/sys/class/hwmon` outside tests.
    hwmon_root: PathBuf,
    timing: Timing,
    /// Parallel to `profile.sensors`: (warning, critical) for each
    /// selector's chip, resolved once from `[temperature]` -- what a
    /// fixed-mode fan's critical override goes by.
    thresholds: Vec<(f32, f32)>,
    /// True while a critical temperature has the fan overridden to
    /// `max_pwm` -- see `update_critical_override`.
    critical_override: bool,
    /// When this fan's own curve was last (re)evaluated -- each profile
    /// has its own `update_secs`, so this is owned per-controller rather
    /// than by a single timer in main.rs's loop.
    last_tick: Option<Instant>,
    /// True once this fan has been confirmed running normally at least
    /// once -- gates stall logging so the very first kick from a cold
    /// daemon start (completely normal) isn't logged as a fault the way a
    /// fan that stops after having run fine would be.
    ever_ran_normally: bool,
    /// How many consecutive ticks this fan has been stalled through,
    /// despite a restart attempt each time -- see `UNRESPONSIVE_AFTER_STALLS`.
    consecutive_stalls: u32,
    /// Whether a `min_expected_rpm` warning is currently active, so the
    /// "back to normal" notice only fires once, on the actual transition.
    low_rpm_warned: bool,
    /// True while every sensor feeding this fan has stopped reading (and
    /// so the fan is being held at full speed instead).
    sensors_lost: bool,
    /// Has any of this fan's selectors ever produced a reading?
    any_sensor_read: bool,
    /// When this controller was created -- the clock for "never took over"
    /// and "never had a sensor" escalation.
    started: Instant,
    /// Set from construction (nothing has been written yet) until a write
    /// succeeds, and again whenever one fails.
    control_loss: Option<ControlLoss>,
    /// The "no sensor ever read" CRITICAL has been logged.
    no_sensor_critical_logged: bool,
}

impl FanController {
    /// One line for the `status` report: name, commanded pwm (raw + %),
    /// live RPM (fresh sysfs read, not cached), and current health. `--`
    /// for RPM if this profile has no `fan_index` configured (no tach to
    /// read) or the chip isn't resolved (not loaded yet).
    pub fn status_line(&self) -> String {
        if !self.profile.enabled {
            let why = if self.profile.disabled_by_config {
                "disabled by config (left in BIOS/driver mode, which may stop the fan)"
            } else {
                "disabled"
            };
            return format!(
                "{} (pwm{}): {why} [{:?}]",
                self.profile.name,
                self.profile.pwm_index,
                self.health_level()
            );
        }
        let pwm = self.last_pwm.unwrap_or(0);
        let pct = u32::from(pwm) * 100 / 255;
        let rpm = match (&self.hwmon, self.profile.fan_index) {
            (Some(hwmon), Some(n)) => read_sysfs_raw_f32(&format!("{hwmon}/fan{n}_input"))
                .map_or_else(|| "--".to_string(), |r| format!("{r:.0}rpm")),
            _ => "--".to_string(),
        };
        let mode = match (self.profile.mode, self.critical_override) {
            (FanMode::Curve, false) => "",
            (FanMode::Curve, true) => " critical-temp override",
            (FanMode::Fixed, false) => " fixed",
            (FanMode::Fixed, true) => " fixed, critical-temp override",
        };
        format!(
            "{} (pwm{}): pwm={pwm} ({pct}%) {rpm} [{:?}]{mode}",
            self.profile.name,
            self.profile.pwm_index,
            self.health_level()
        )
    }

    /// Current health, for the status LED (`state::recompute_status_led`)
    /// -- current state, not transition-gated the way this fan's own
    /// syslog lines are (the LED always reflects "right now").
    pub fn health_level(&self) -> Level {
        if self.profile.disabled_by_config {
            return Level::Warn;
        }
        let uncontrolled_for = self.control_loss.as_ref().map(|l| l.since.elapsed());
        let never_had_a_sensor = self.sensors_lost && !self.any_sensor_read;
        if self.consecutive_stalls >= UNRESPONSIVE_AFTER_STALLS
            || uncontrolled_for.is_some_and(|t| t >= self.timing.control_critical_after)
            || (never_had_a_sensor && self.started.elapsed() >= self.timing.control_critical_after)
        {
            Level::Critical
        } else if uncontrolled_for.is_some_and(|t| t >= self.timing.control_warn_after)
            || self.consecutive_stalls > 0
            || self.low_rpm_warned
            || self.sensors_lost
            || self.floor_active()
        {
            Level::Warn
        } else {
            Level::Info
        }
    }

    /// An enabled fan starts out "not under control" until its first
    /// successful write; a disabled one never is.
    fn starting_unmanaged(mut self) -> Self {
        if self.profile.enabled {
            self.control_loss = Some(ControlLoss {
                since: self.started,
                why: "not taken over yet".to_string(),
                warned: false,
                critical: false,
            });
        }
        self
    }

    pub fn new(profile: FanProfile, temperature: &TemperatureConfig) -> Self {
        let feeds = profile
            .sensors
            .iter()
            .map(|sel| SensorFeed::new(sel, profile.update_secs))
            .collect();
        let thresholds = profile
            .sensors
            .iter()
            .map(|sel| resolve_temp_threshold(temperature, &sel.chip))
            .collect();
        FanController {
            profile,
            hwmon: None,
            last_pwm: None,
            kick_until: None,
            feeds,
            hwmon_root: PathBuf::from("/sys/class/hwmon"),
            timing: Timing::default(),
            thresholds,
            critical_override: false,
            last_tick: None,
            ever_ran_normally: false,
            consecutive_stalls: 0,
            low_rpm_warned: false,
            sensors_lost: false,
            any_sensor_read: false,
            started: Instant::now(),
            control_loss: None,
            no_sensor_critical_logged: false,
        }
        .starting_unmanaged()
    }

    /// Call every time main.rs's event loop wakes up (it polls at a fixed
    /// ~100ms ceiling); a no-op except once every `profile.update_secs`.
    pub fn tick(&mut self) {
        let interval = Duration::from_secs(self.profile.update_secs.max(1));
        let due = match self.last_tick {
            None => true,
            Some(t) => t.elapsed() >= interval,
        };
        if !due {
            return;
        }
        self.last_tick = Some(Instant::now());
        self.update();
    }

    /// Best-effort, like the rest of the hardware glue here: any failure
    /// (module not loaded yet, hwmon renumbered after a reload, a
    /// permission problem) is swallowed and retried next tick rather than
    /// taking the whole daemon down.
    fn update(&mut self) {
        if !self.profile.enabled {
            return;
        }

        if self.hwmon.is_none() {
            self.hwmon = self.resolve_hwmon();
        }
        let Some(hwmon) = self.hwmon.clone() else {
            // Chip not loaded (yet) -- try again next tick, but never
            // silently: see `note_control_lost`.
            self.note_control_lost(&format!("pwm chip '{}' not found", self.profile.pwm_chip));
            return;
        };

        let Some(sensor_target) = self.target_pwm_from_sensors() else {
            self.hold_full_speed_without_sensors(&hwmon);
            return;
        };
        if self.sensors_lost {
            self.sensors_lost = false;
            crate::syslog::notice(&format!(
                "fan '{}' (pwm{}): sensor readings back, resuming curve",
                self.profile.name, self.profile.pwm_index
            ));
        }

        // Mid-kick: leave min_start_pwm in place until it's had time to
        // actually get the fan spinning, then fall through to a normal
        // computation on the next call once it elapses.
        if let Some(until) = self.kick_until {
            if Instant::now() < until {
                return;
            }
            self.kick_until = None;
        }

        let rpm = self
            .profile
            .fan_index
            .and_then(|n| read_sysfs_raw_f32(&format!("{hwmon}/fan{n}_input")));

        let cmd = self.plan(sensor_target, rpm);
        self.apply(&hwmon, cmd);
    }

    /// Writes `cmd`, asserting manual mode first -- every time, for kicks
    /// too. In automatic mode the driver rejects `pwmN` writes outright,
    /// so a kick that skipped this could never take the fan over (and
    /// would mistake the rejection for a stale hwmon path, forever). Some
    /// firmware also resets `pwmN_enable` back to automatic on its own;
    /// fancontrol(8) re-asserts it every tick for the same reason.
    fn apply(&mut self, hwmon: &str, cmd: PwmCommand) {
        let (PwmCommand::Set(pwm) | PwmCommand::Kick(pwm)) = cmd;
        if self.ensure_manual_mode(hwmon).is_err() || self.write_pwm(hwmon, pwm).is_err() {
            self.hwmon = None; // path went stale -- re-resolve next tick
            self.note_control_lost(&format!("writing pwm{} failed", self.profile.pwm_index));
            return;
        }
        self.mark_control_ok();
        self.last_pwm = Some(pwm);
        if matches!(cmd, PwmCommand::Kick(_)) {
            self.kick_until = Some(Instant::now() + Duration::from_secs(1));
        }
    }

    /// Decides what to write given the curve's `target` and the tach
    /// reading, updating stall/low-RPM tracking (and logging transitions)
    /// along the way. No I/O of its own -- the caller writes the result
    /// and records it in `last_pwm` only if the write succeeds.
    ///
    /// A `target` of 0 (`min_pwm = 0`, below `min_temp_c`) is a deliberate
    /// stop, not a stall. A stopped fan that the curve now wants spinning
    /// is kicked with `min_start_pwm` first, as upstream fancontrol does.
    /// A *stall* is narrower: the fan was last commanded to spin, yet a
    /// tach reading exists and is zero ("no reading" / "no tach" is not
    /// evidence of a stall).
    fn plan(&mut self, target: u8, rpm: Option<f32>) -> PwmCommand {
        if target == 0 {
            self.consecutive_stalls = 0;
            self.low_rpm_warned = false;
            return PwmCommand::Set(0);
        }

        let commanded = self.last_pwm.unwrap_or(0);
        let stalled = commanded > 0 && matches!(rpm, Some(r) if r <= 0.0);
        if stalled {
            self.consecutive_stalls += 1;
            // Not logged until this fan has run normally at least once --
            // right after a cold start the tach can still read 0 for a
            // moment after the first kick, which isn't a fault.
            if self.ever_ran_normally {
                if self.consecutive_stalls == 1 {
                    crate::syslog::warning(&format!(
                        "fan '{}' (pwm{}): not spinning, attempting restart",
                        self.profile.name, self.profile.pwm_index
                    ));
                } else if self.consecutive_stalls == UNRESPONSIVE_AFTER_STALLS {
                    crate::syslog::critical(&format!(
                        "fan '{}' (pwm{}): unresponsive after {} restart attempts",
                        self.profile.name, self.profile.pwm_index, self.consecutive_stalls
                    ));
                }
            }
            return PwmCommand::Kick(self.profile.min_start_pwm);
        }
        if commanded == 0 {
            return PwmCommand::Kick(self.profile.min_start_pwm);
        }

        if self.consecutive_stalls > 0 {
            if self.ever_ran_normally {
                crate::syslog::notice(&format!(
                    "fan '{}' (pwm{}): spinning again after {} restart attempt(s)",
                    self.profile.name, self.profile.pwm_index, self.consecutive_stalls
                ));
            }
            self.consecutive_stalls = 0;
        }
        self.ever_ran_normally = true;

        if let (Some(min_rpm), Some(rpm)) = (self.profile.min_expected_rpm, rpm) {
            let low = f64::from(rpm) < f64::from(min_rpm);
            if low && !self.low_rpm_warned {
                self.low_rpm_warned = true;
                crate::syslog::warning(&format!(
                    "fan '{}' (pwm{}): {rpm:.0} RPM, below expected minimum ({min_rpm} RPM)",
                    self.profile.name, self.profile.pwm_index
                ));
            } else if !low && self.low_rpm_warned {
                self.low_rpm_warned = false;
                crate::syslog::notice(&format!(
                    "fan '{}' (pwm{}): back to {rpm:.0} RPM, at or above expected minimum ({min_rpm} RPM)",
                    self.profile.name, self.profile.pwm_index
                ));
            }
        }

        PwmCommand::Set(target)
    }

    /// The PWM this fan should run at right now, per its `mode`; `None` if
    /// no sensor feeding it has a reading. While any selector is lost
    /// (`floor_active`) a fan that has readings still runs at `max_pwm`.
    fn target_pwm_from_sensors(&mut self) -> Option<u8> {
        self.track_sensors();
        let target = match self.profile.mode {
            FanMode::Curve => self.curve_target(),
            FanMode::Fixed => self.fixed_target(),
        };
        if self.floor_active() {
            target.map(|_| self.profile.max_pwm)
        } else {
            target
        }
    }

    /// True while some selector that used to read has been silent past the
    /// grace (and hasn't yet read steadily again): the fan can't know what
    /// that sensor would say, so it is held at `max_pwm`.
    fn floor_active(&self) -> bool {
        self.feeds.iter().any(|f| f.held)
    }

    /// Updates each selector's loss/recovery state from its latest sample,
    /// logging once per transition. Called each time the fan is updated.
    fn track_sensors(&mut self) {
        let fan = format!(
            "fan '{}' (pwm{})",
            self.profile.name, self.profile.pwm_index
        );
        let now = Instant::now();
        let mut any_read = false;
        for (sel, feed) in self.profile.sensors.iter().zip(&mut self.feeds) {
            if feed.current().is_some() {
                any_read = true;
                feed.ever_read = true;
                feed.lost_since = None;
                if feed.held {
                    let since = *feed.back_since.get_or_insert(now);
                    if now.saturating_duration_since(since) >= self.timing.sensor_recovery_hold {
                        feed.held = false;
                        feed.back_since = None;
                        crate::syslog::notice(&format!(
                            "{fan}: sensor '{}' reading steadily again, resuming normal control",
                            sel.chip
                        ));
                    }
                }
            } else if feed.ever_read {
                feed.back_since = None;
                let since = *feed.lost_since.get_or_insert(now);
                if !feed.held
                    && now.saturating_duration_since(since) >= self.timing.sensor_loss_grace
                {
                    feed.held = true;
                    crate::syslog::warning(&format!(
                        "{fan}: sensor '{}' stopped reading, holding max_pwm until it reads \
                         steadily again",
                        sel.chip
                    ));
                }
            }
        }
        self.any_sensor_read |= any_read;
    }

    /// Starts one reader thread per sensor selector (see `SensorFeed`).
    /// They run until `stop` is set. A thread that can't be started is
    /// logged; its selector then never reads, which the loss handling
    /// treats like any other missing sensor.
    fn start_readers(&self, stop: &Arc<AtomicBool>) {
        if !self.profile.enabled {
            return;
        }
        for (sel, feed) in self.profile.sensors.iter().zip(&self.feeds) {
            let interval = Duration::from_secs(sampling_secs(sel, self.profile.update_secs));
            let (root, selector) = (self.hwmon_root.clone(), sel.clone());
            let result = spawn_reader(
                format!("sensor-{}", sel.chip),
                interval,
                Arc::clone(stop),
                Arc::clone(&feed.shared),
                move || {
                    resolve_selector_in(&root, &selector)
                        .into_iter()
                        .reduce(f32::max)
                },
            );
            if let Err(e) = result {
                crate::syslog::critical(&format!(
                    "fan '{}' (pwm{}): could not start the reader for sensor '{}': {e}",
                    self.profile.name, self.profile.pwm_index, sel.chip
                ));
            }
        }
    }

    /// Pegs the fan at `max_pwm` from the moment any sensor reaches its
    /// critical threshold until every sensor is back below its *warning*
    /// threshold. The gap between the two is hysteresis, so a sensor
    /// hovering at its critical point doesn't flip the fan between quiet
    /// and full speed every few seconds -- and once something has gotten
    /// that hot, it should be properly cooled, not just nudged back under
    /// the line. Logged on both transitions. Returns whether it's active.
    fn update_critical_override(&mut self, readings: &[(usize, f32)]) -> bool {
        // An over-range reading (see `hal::is_over_range`) is hot whatever
        // the configured thresholds say.
        let hot = readings
            .iter()
            .copied()
            .find(|&(i, t)| t >= self.thresholds[i].1 || is_over_range(t));
        if let Some((i, temp_c)) = hot {
            if !self.critical_override {
                self.critical_override = true;
                let what = match self.profile.mode {
                    FanMode::Curve => "the curve",
                    FanMode::Fixed => "fixed pwm",
                };
                crate::syslog::warning(&format!(
                    "fan '{}' (pwm{}): {} at {temp_c:.1}C{}, at/above its critical {:.1}C -- \
                     overriding {what} to max_pwm until everything is below warning",
                    self.profile.name,
                    self.profile.pwm_index,
                    self.profile.sensors[i].chip,
                    if is_over_range(temp_c) {
                        " (over-range: failed sensor or real emergency)"
                    } else {
                        ""
                    },
                    self.thresholds[i].1
                ));
            }
        } else if self.critical_override
            && readings
                .iter()
                .all(|&(i, t)| t < self.thresholds[i].0 && !is_over_range(t))
        {
            self.critical_override = false;
            crate::syslog::notice(&format!(
                "fan '{}' (pwm{}): all sensors below warning, back to normal control",
                self.profile.name, self.profile.pwm_index
            ));
        }
        self.critical_override
    }

    /// (sensor index, latest temperature) for every sensor with a reading.
    fn readings(&self) -> Vec<(usize, f32)> {
        self.feeds
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.current().map(|t| (i, t)))
            .collect()
    }

    /// Curve mode: the PWM demanded by the *hottest-demanding* sensor --
    /// not simply "feed the hottest raw temperature through one shared
    /// curve". Each sensor selector can define its own
    /// `min_temp_c`/`max_temp_c` (falling back to the fan profile's own if
    /// unset), so e.g. a drive can be configured to hit `max_pwm` at its
    /// own critical threshold (60C) even though the fan's CPU-tuned curve
    /// doesn't reach `max_temp_c` until 90C. Evaluating each sensor against
    /// its own curve and taking the worst *resulting PWM* (not the worst
    /// raw temperature first) is what makes that actually work: a drive at
    /// 61C against a 45-90C shared curve would only compute to a modest
    /// partial speed, nowhere near the full-speed response its own 60C
    /// critical threshold warrants. On top of that, a sensor at its critical
    /// threshold pegs the fan regardless (`update_critical_override`).
    fn curve_target(&mut self) -> Option<u8> {
        let readings = self.readings();
        if readings.is_empty() {
            return None;
        }
        if self.update_critical_override(&readings) {
            return Some(self.profile.max_pwm);
        }
        readings
            .into_iter()
            .map(|(i, temp_c)| {
                let sel = &self.profile.sensors[i];
                let min_t = sel.min_temp_c.unwrap_or(self.profile.min_temp_c);
                let max_t = sel.max_temp_c.unwrap_or(self.profile.max_temp_c);
                compute_pwm(temp_c, min_t, max_t, &self.profile)
            })
            .max()
    }

    /// Fixed mode: `fixed_pwm`, unless a critical temperature has
    /// `update_critical_override` pegging the fan at `max_pwm`.
    fn fixed_target(&mut self) -> Option<u8> {
        let readings = self.readings();
        if readings.is_empty() {
            return None;
        }
        Some(if self.update_critical_override(&readings) {
            self.profile.max_pwm
        } else {
            // Validated present for an enabled fixed-mode fan; full speed
            // is the safe answer if it somehow isn't.
            self.profile.fixed_pwm.unwrap_or(self.profile.max_pwm)
        })
    }

    /// No sensor feeding this fan has a reading. Right after start (sensor
    /// modules not loaded yet) the fan is left alone, in whatever mode the
    /// BIOS/driver set, for `sensor_takeover_after`: that is normal at
    /// boot. Past that -- or once this controller has taken the fan over
    /// and then lost every sensor (a module unloaded, a chip renumbered) --
    /// fail safe to `max_pwm`. Leaving BIOS automatic mode in charge is not
    /// safe here: on this board's it8625 it stops the fan outright.
    fn hold_full_speed_without_sensors(&mut self, hwmon: &str) {
        let first_takeover = self.last_pwm.is_none();
        if first_takeover && self.started.elapsed() < self.timing.sensor_takeover_after {
            return;
        }
        if !self.sensors_lost {
            self.sensors_lost = true;
            let fan = format!(
                "fan '{}' (pwm{})",
                self.profile.name, self.profile.pwm_index
            );
            if first_takeover {
                crate::syslog::warning(&format!(
                    "{fan}: no sensor has produced a reading {}s after start; taking the fan \
                     over at max_pwm rather than leaving BIOS automatic mode in charge \
                     (which can stop it)",
                    self.started.elapsed().as_secs()
                ));
            } else {
                crate::syslog::warning(&format!("{fan}: no sensor readings, holding at max_pwm"));
            }
        }
        if !self.any_sensor_read
            && !self.no_sensor_critical_logged
            && self.started.elapsed() >= self.timing.control_critical_after
        {
            self.no_sensor_critical_logged = true;
            crate::syslog::critical(&format!(
                "fan '{}' (pwm{}): still no sensor has ever produced a reading \
                 ({}s); check its [[fans.sensors]] selectors; fan held at max_pwm",
                self.profile.name,
                self.profile.pwm_index,
                self.started.elapsed().as_secs()
            ));
        }
        self.kick_until = None;
        let max = self.profile.max_pwm;
        if self.ensure_manual_mode(hwmon).is_err() || self.write_pwm(hwmon, max).is_err() {
            self.hwmon = None;
            self.note_control_lost(&format!("writing pwm{} failed", self.profile.pwm_index));
            return;
        }
        self.mark_control_ok();
        self.last_pwm = Some(max);
    }

    /// This enabled fan can't be driven right now (its pwm chip isn't
    /// there, or a write failed). Normal for a while at boot, so silent
    /// until `control_warn_after`; then one WARNING, and one CRITICAL after
    /// `control_critical_after` -- the fan is in whatever mode the BIOS
    /// left it in, which may be stopped. Health follows the same clock.
    fn note_control_lost(&mut self, why: &str) {
        let fan = format!(
            "fan '{}' (pwm{})",
            self.profile.name, self.profile.pwm_index
        );
        let (warn_after, critical_after) = (
            self.timing.control_warn_after,
            self.timing.control_critical_after,
        );
        let loss = self.control_loss.get_or_insert_with(|| ControlLoss {
            since: Instant::now(),
            why: String::new(),
            warned: false,
            critical: false,
        });
        loss.why = why.to_string();
        let age = loss.since.elapsed();
        if age >= critical_after && !loss.critical {
            loss.critical = true;
            loss.warned = true;
            crate::syslog::critical(&format!(
                "{fan}: still not under control after {}s: {why}; BIOS/driver automatic \
                 mode may have the fan stopped",
                age.as_secs()
            ));
        } else if age >= warn_after && !loss.warned {
            loss.warned = true;
            crate::syslog::warning(&format!(
                "{fan}: not under control after {}s: {why}; BIOS/driver automatic mode may \
                 leave the fan stopped",
                age.as_secs()
            ));
        }
    }

    /// A write just succeeded: the fan is under control. Logs the
    /// recovery if the loss had been reported.
    fn mark_control_ok(&mut self) {
        if let Some(loss) = self.control_loss.take()
            && loss.warned
        {
            crate::syslog::notice(&format!(
                "fan '{}' (pwm{}): under control again after {}s ({})",
                self.profile.name,
                self.profile.pwm_index,
                loss.since.elapsed().as_secs(),
                loss.why
            ));
        }
    }

    /// Leaves the fan safe on exit (clean shutdown, or unwinding from a
    /// panic -- this runs from `Drop`): manual mode at full speed. Not the
    /// mode found at startup -- on this board's it8625, automatic mode
    /// stops the fan outright (confirmed live: 0 RPM at `pwm1_enable=2`),
    /// so "restoring" it would hand back a stopped fan with nothing
    /// watching temperatures. Full speed is loud but always safe, and the
    /// next daemon start takes it straight back over.
    fn restore(&mut self) {
        if self.last_pwm.is_none() {
            return; // never took control
        }
        let Some(hwmon) = self.hwmon.clone().or_else(|| self.resolve_hwmon()) else {
            return;
        };
        if self.ensure_manual_mode(&hwmon).is_ok() && self.write_pwm(&hwmon, u8::MAX).is_ok() {
            crate::syslog::info(&format!(
                "fan '{}' (pwm{}): left at full speed on exit",
                self.profile.name, self.profile.pwm_index
            ));
        }
    }

    fn resolve_hwmon(&self) -> Option<String> {
        glob_hwmon_in(&self.hwmon_root, &self.profile.pwm_chip).and_then(|v| v.into_iter().next())
    }

    fn enable_path(&self, hwmon: &str) -> String {
        format!("{hwmon}/pwm{}_enable", self.profile.pwm_index)
    }

    fn ensure_manual_mode(&self, hwmon: &str) -> std::io::Result<()> {
        std::fs::write(self.enable_path(hwmon), "1")
    }

    fn write_pwm(&self, hwmon: &str, value: u8) -> std::io::Result<()> {
        std::fs::write(
            format!("{hwmon}/pwm{}", self.profile.pwm_index),
            value.to_string(),
        )
    }
}

impl Drop for FanController {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Ported from lm-sensors' `fancontrol(8)` `UpdateFanSpeeds`: flat
/// `min_pwm` at/below `min_temp_c`, flat `max_pwm` at/above `max_temp_c`,
/// and in between a straight line from `min_stop_pwm` (not `min_pwm`) at
/// `min_temp_c` up to `max_pwm` at `max_temp_c`. `min_temp_c`/`max_temp_c`
/// are passed explicitly rather than always read from `cfg` -- a sensor
/// selector can override the fan profile's own curve endpoints (see
/// `target_pwm_from_sensors`), so this needs to evaluate against whichever
/// pair actually applies to the sensor currently being computed.
/// `min_stop_pwm`/`min_pwm`/`max_pwm` stay profile-level always -- those
/// describe the fan itself, not any particular sensor.
///
/// Doesn't handle the stall/kick case -- that's `FanController::update`'s
/// job, since it needs mutable state (`kick_until`) this pure function
/// doesn't have.
fn compute_pwm(temp_c: f32, min_temp_c: f32, max_temp_c: f32, cfg: &FanProfile) -> u8 {
    let raw = if temp_c <= min_temp_c {
        f32::from(cfg.min_pwm)
    } else if temp_c >= max_temp_c {
        f32::from(cfg.max_pwm)
    } else {
        let (min_stop, max_pwm) = (f32::from(cfg.min_stop_pwm), f32::from(cfg.max_pwm));
        (temp_c - min_temp_c) * (max_pwm - min_stop) / (max_temp_c - min_temp_c) + min_stop
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255 first"
    )]
    let pwm = raw.round().clamp(0.0, 255.0) as u8;
    pwm
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default curve (45-90C, pwm 50/55/60..255) on it8625 pwm1/fan1.
    fn cfg() -> FanProfile {
        FanProfile {
            name: "test".to_string(),
            pwm_chip: "it8625".to_string(),
            fan_index: Some(1),
            ..FanProfile::default()
        }
    }

    /// Evaluates against `cfg()`'s own `min_temp_c`/`max_temp_c` (45/90) --
    /// the common case, a sensor with no per-selector override.
    fn pwm(temp_c: f32) -> u8 {
        let c = cfg();
        compute_pwm(temp_c, c.min_temp_c, c.max_temp_c, &c)
    }

    #[test]
    fn at_or_below_min_temp_is_flat_min_pwm() {
        assert_eq!(pwm(30.0), 50);
        assert_eq!(pwm(45.0), 50);
    }

    #[test]
    fn at_or_above_max_temp_is_flat_max_pwm() {
        assert_eq!(pwm(90.0), 255);
        assert_eq!(pwm(120.0), 255);
    }

    #[test]
    fn just_above_min_temp_starts_near_min_stop_not_min_pwm() {
        // The ramp's intercept at min_temp_c is min_stop_pwm(55), not
        // min_pwm(50) -- this is the non-obvious bit the graph in
        // fancontrol.txt documents.
        let p = pwm(45.01);
        assert!((54..=56).contains(&p), "got {p}");
    }

    #[test]
    fn midpoint_interpolates_from_min_stop_to_max_pwm() {
        // halfway between 45 and 90 -> halfway between min_stop(55) and 255
        let p = pwm(67.5);
        let expected = 55 + (255 - 55) / 2;
        assert!((i32::from(p) - expected).abs() <= 1, "got {p}");
    }

    #[test]
    fn matches_the_actual_curve_used_before_this_replaced_fancontrol() {
        // Spot-check against values observed live (via /etc/fancontrol,
        // the same curve) on nas.skycorgi.net before this daemon took
        // over: modest CPU load kept pwm1 in the 100-200 range.
        let p = pwm(60.0);
        assert!((100..=170).contains(&p), "got {p}");
    }

    #[test]
    fn matches_live_observed_value_at_67c() {
        // CPU package temp 67C observed live on nas.skycorgi.net produced
        // pwm1=153 through this exact formula -- pinned here so a future
        // change to the algorithm has to justify moving this number.
        assert_eq!(pwm(67.0), 153);
    }

    #[test]
    fn per_sensor_override_hits_max_pwm_at_its_own_threshold_not_the_profiles() {
        // The actual bug this exists to prevent: a drive at its own 60C
        // critical threshold must reach max_pwm even though the fan
        // profile's own (CPU-tuned) max_temp_c is 90 -- a shared curve
        // would only compute a modest partial speed at 60C, nowhere near
        // the full-speed response the drive's own danger zone warrants.
        let c = cfg();
        let drive_min_t = 50.0; // drivetemp warn threshold
        let drive_max_t = 60.0; // drivetemp critical threshold
        assert_eq!(compute_pwm(60.0, drive_min_t, drive_max_t, &c), 255);
        assert_eq!(compute_pwm(70.0, drive_min_t, drive_max_t, &c), 255); // past its own max too
    }

    impl FanController {
        /// Publishes `values` as each selector's fresh sample, as its
        /// reader thread would.
        fn set_samples(&mut self, values: &[Option<f32>]) {
            for (feed, value) in self.feeds.iter_mut().zip(values) {
                *lock(&feed.shared) = Sample {
                    at: Some(Instant::now()),
                    value: *value,
                };
            }
        }
    }

    /// A scratch directory standing in for a hwmon device, seeded with
    /// `pwm1`/`pwm1_enable`. Removed on drop.
    struct FakeHwmon(std::path::PathBuf);

    impl FakeHwmon {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("lcm-status-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("pwm1"), "120").unwrap();
            std::fs::write(dir.join("pwm1_enable"), "2").unwrap();
            FakeHwmon(dir)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.0.join(file)).unwrap()
        }
    }

    impl Drop for FakeHwmon {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A controller already pointed at `hw`, with no sensors configured
    /// (so every `update` sees "no readings").
    fn controller_on(hw: &FakeHwmon) -> FanController {
        let mut c = new_controller(FanProfile {
            fan_index: None,
            ..cfg()
        });
        c.hwmon = Some(hw.path());
        c
    }

    #[test]
    fn losing_all_sensors_after_taking_control_holds_max_pwm() {
        let hw = FakeHwmon::new("lost");
        let mut c = controller_on(&hw);
        c.last_pwm = Some(100);
        c.update();
        assert_eq!(hw.read("pwm1"), "255");
        assert_eq!(hw.read("pwm1_enable"), "1");
        assert_eq!(c.health_level(), Level::Warn);
    }

    #[test]
    fn no_sensors_before_taking_control_leaves_the_fan_alone() {
        let hw = FakeHwmon::new("untouched");
        let mut c = controller_on(&hw);
        c.update();
        assert_eq!(hw.read("pwm1"), "120");
        assert_eq!(hw.read("pwm1_enable"), "2");
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn drop_leaves_the_fan_manual_at_full_speed() {
        let hw = FakeHwmon::new("restore");
        let mut c = controller_on(&hw);
        c.last_pwm = Some(100);
        drop(c);
        assert_eq!(hw.read("pwm1_enable"), "1");
        assert_eq!(hw.read("pwm1"), "255");
    }

    #[test]
    fn drop_without_ever_taking_control_touches_nothing() {
        let hw = FakeHwmon::new("never");
        drop(controller_on(&hw));
        assert_eq!(hw.read("pwm1_enable"), "2");
        assert_eq!(hw.read("pwm1"), "120");
    }

    #[test]
    fn kick_switches_to_manual_mode_before_writing() {
        // FakeHwmon starts in automatic mode (pwm1_enable=2), where the
        // real driver rejects pwm writes.
        let hw = FakeHwmon::new("kick");
        let mut c = controller_on(&hw);
        c.apply(&hw.path(), PwmCommand::Kick(60));
        assert_eq!(hw.read("pwm1_enable"), "1");
        assert_eq!(hw.read("pwm1"), "60");
        assert_eq!(c.last_pwm, Some(60));
        assert!(c.kick_until.is_some());
    }

    fn new_controller(profile: FanProfile) -> FanController {
        FanController::new(profile, &TemperatureConfig::default())
    }

    fn plan_controller() -> FanController {
        new_controller(cfg())
    }

    #[test]
    fn intentional_stop_is_not_a_stall() {
        // min_pwm = 0: the curve asks for 0 below min_temp_c. The fan must
        // stay stopped -- no kick/stop oscillation, no stall alerts.
        let mut c = plan_controller();
        c.ever_ran_normally = true;
        c.last_pwm = Some(0);
        for _ in 0..5 {
            assert_eq!(c.plan(0, Some(0.0)), PwmCommand::Set(0));
            c.last_pwm = Some(0);
        }
        assert_eq!(c.consecutive_stalls, 0);
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn restarting_from_an_intentional_stop_kicks_without_counting_a_stall() {
        let mut c = plan_controller();
        c.ever_ran_normally = true;
        c.last_pwm = Some(0);
        assert_eq!(c.plan(100, Some(0.0)), PwmCommand::Kick(60));
        assert_eq!(c.consecutive_stalls, 0);
    }

    #[test]
    fn cold_start_kicks_first() {
        let mut c = plan_controller();
        assert_eq!(c.plan(100, None), PwmCommand::Kick(60));
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn real_stall_escalates_to_critical_and_recovers() {
        let mut c = plan_controller();
        c.ever_ran_normally = true;
        c.last_pwm = Some(150);
        assert_eq!(c.plan(150, Some(0.0)), PwmCommand::Kick(60));
        assert_eq!(c.health_level(), Level::Warn);
        c.last_pwm = Some(60);
        c.plan(150, Some(0.0));
        c.plan(150, Some(0.0));
        assert_eq!(c.health_level(), Level::Critical);

        assert_eq!(c.plan(150, Some(1200.0)), PwmCommand::Set(150));
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn missing_tach_reading_is_not_a_stall() {
        let mut c = plan_controller();
        c.last_pwm = Some(150);
        assert_eq!(c.plan(150, None), PwmCommand::Set(150));
        assert_eq!(c.consecutive_stalls, 0);
    }

    #[test]
    fn per_sensor_override_still_flat_min_pwm_below_its_own_min() {
        let c = cfg();
        assert_eq!(compute_pwm(40.0, 50.0, 60.0, &c), 50); // below the drive's own min_temp_c
    }
    /// A fixed-mode fan at pwm 100, fed by drivetemp (warn 50 / crit 60 by
    /// default) and coretemp (85 / 100).
    fn fixed_controller() -> FanController {
        new_controller(FanProfile {
            mode: FanMode::Fixed,
            fixed_pwm: Some(100),
            sensors: vec![
                SensorSelector {
                    chip: "drivetemp".into(),
                    ..SensorSelector::default()
                },
                SensorSelector {
                    chip: "coretemp".into(),
                    ..SensorSelector::default()
                },
            ],
            ..cfg()
        })
    }

    /// Feeds `fixed_target` these readings, as if just sampled.
    fn fixed_with(c: &mut FanController, drive: Option<f32>, cpu: Option<f32>) -> Option<u8> {
        c.set_samples(&[drive, cpu]);
        c.fixed_target()
    }

    #[test]
    fn fixed_mode_ignores_the_curve() {
        let mut c = fixed_controller();
        // 80C CPU would be well up the 45-90C curve; fixed doesn't care.
        assert_eq!(fixed_with(&mut c, Some(40.0), Some(80.0)), Some(100));
    }

    #[test]
    fn fixed_mode_goes_to_max_at_a_critical_temp_until_below_warning() {
        let mut c = fixed_controller();
        assert_eq!(fixed_with(&mut c, Some(60.0), Some(50.0)), Some(255));
        // Back under critical, but still above the drive's 50C warning.
        assert_eq!(fixed_with(&mut c, Some(55.0), Some(50.0)), Some(255));
        assert!(c.status_line().contains("critical-temp override"));
        assert_eq!(fixed_with(&mut c, Some(49.0), Some(50.0)), Some(100));
        // The CPU's critical point is its own (100C), not the drive's.
        assert_eq!(fixed_with(&mut c, Some(40.0), Some(99.0)), Some(100));
        assert_eq!(fixed_with(&mut c, Some(40.0), Some(100.0)), Some(255));
    }

    /// A curve-mode fan whose drive endpoints are deliberately *above* the
    /// drive's critical threshold (60C), so only the override can peg it.
    fn curve_controller() -> FanController {
        new_controller(FanProfile {
            sensors: vec![
                SensorSelector {
                    chip: "drivetemp".into(),
                    min_temp_c: Some(50.0),
                    max_temp_c: Some(80.0),
                    ..SensorSelector::default()
                },
                SensorSelector {
                    chip: "coretemp".into(),
                    ..SensorSelector::default()
                },
            ],
            ..cfg()
        })
    }

    fn curve_with(c: &mut FanController, drive: Option<f32>, cpu: Option<f32>) -> Option<u8> {
        c.set_samples(&[drive, cpu]);
        c.curve_target()
    }

    #[test]
    fn curve_mode_pegs_at_a_critical_temp_whatever_the_curve_endpoints() {
        let mut c = curve_controller();
        let below = curve_with(&mut c, Some(59.0), Some(40.0)).unwrap();
        assert!(below < 255, "got {below}");
        assert_eq!(curve_with(&mut c, Some(60.0), Some(40.0)), Some(255));
        assert!(c.status_line().contains("critical-temp override"));
        // Still pegged under critical but above the drive's 50C warning...
        assert_eq!(curve_with(&mut c, Some(55.0), Some(40.0)), Some(255));
        // ...and back on the curve once everything is below warning.
        let after = curve_with(&mut c, Some(49.0), Some(40.0)).unwrap();
        assert!(after < 255, "got {after}");
        assert!(!c.status_line().contains("override"));
    }

    #[test]
    fn an_over_range_reading_pegs_the_fan_even_above_misconfigured_thresholds() {
        // Thresholds nobody could reach (warn/critical above the plausible
        // range) must not hide a 130C reading.
        let temperature = TemperatureConfig {
            warn_threshold: 500.0,
            critical_threshold: 600.0,
            ..TemperatureConfig::default()
        };
        let mut c = FanController::new(
            FanProfile {
                sensors: vec![SensorSelector {
                    chip: "coretemp".into(),
                    ..SensorSelector::default()
                }],
                ..cfg()
            },
            &temperature,
        );
        c.set_samples(&[Some(130.0)]);
        assert_eq!(c.curve_target(), Some(255));
        assert!(c.status_line().contains("critical-temp override"));
        // Back in range: released (the huge warning threshold is cleared).
        c.set_samples(&[Some(60.0)]);
        assert!(c.curve_target().unwrap() < 255);
    }

    #[test]
    fn a_hot_nic_pegs_the_default_fan() {
        // The AQC113's PHY/MAC sensor: critical at 100C, ADM's LAN curve.
        let cfg = crate::config::Config::default();
        let temperature = &cfg.temperature;
        let mut c = FanController::new(cfg.fans[0].clone(), temperature);
        let nic = cfg.fans[0]
            .sensors
            .iter()
            .position(|s| s.chip == "enp9s0")
            .expect("default fan has an AQC113 sensor");
        let mut feed = |temp: Option<f32>| {
            let mut samples = vec![None; c.feeds.len()];
            samples[nic] = temp;
            c.set_samples(&samples);
            c.curve_target()
        };
        let warm = feed(Some(72.0)).unwrap();
        assert!(warm < 255, "got {warm}");
        assert_eq!(feed(Some(100.0)), Some(255));
        assert_eq!(feed(Some(85.0)), Some(255)); // above warning: stays pegged
    }

    #[test]
    fn fixed_mode_with_no_readings_takes_the_sensors_lost_path() {
        let mut c = fixed_controller();
        assert_eq!(fixed_with(&mut c, None, None), None);
    }

    #[test]
    fn fixed_mode_still_kicks_and_detects_stalls() {
        let mut c = fixed_controller();
        assert_eq!(c.plan(100, None), PwmCommand::Kick(60));
        c.ever_ran_normally = true;
        c.last_pwm = Some(100);
        assert_eq!(c.plan(100, Some(0.0)), PwmCommand::Kick(60));
        assert_eq!(c.health_level(), Level::Warn);
    }

    /// A controller on `hw` fed by two selectors ("cpu", "drive"), with
    /// the loss grace shrunk to nothing and a short recovery hold.
    fn two_sensor_controller(hw: &FakeHwmon) -> FanController {
        let sel = |chip: &str| SensorSelector {
            chip: chip.into(),
            ..SensorSelector::default()
        };
        let mut c = new_controller(FanProfile {
            fan_index: None,
            sensors: vec![sel("cpu"), sel("drive")],
            ..cfg()
        });
        c.hwmon = Some(hw.path());
        c.timing = Timing {
            sensor_loss_grace: Duration::ZERO,
            sensor_recovery_hold: Duration::from_millis(80),
            ..Timing::default()
        };
        c
    }

    fn pwm_written(hw: &FakeHwmon) -> u8 {
        hw.read("pwm1").trim().parse().unwrap()
    }

    #[test]
    fn a_wedged_sensor_read_cannot_freeze_the_control_path() {
        let hw = FakeHwmon::new("wedged");
        let mut c = two_sensor_controller(&hw);
        // The drive selector's reader blocks forever (a hung drivetemp
        // read); only the sender's drop at the end of the test frees it.
        let (_release, wedge) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        spawn_reader(
            "wedged".into(),
            Duration::from_secs(1),
            Arc::clone(&stop),
            Arc::clone(&c.feeds[1].shared),
            move || {
                let _ = wedge.recv();
                None
            },
        )
        .unwrap();
        // CPU keeps producing: the curve still ticks and writes.
        c.last_pwm = Some(100);
        c.set_samples(&[Some(80.0), None]);
        let started = Instant::now();
        c.update();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(pwm_written(&hw) > 150, "got {}", pwm_written(&hw));
        // The never-read drive is merely absent, not a fault.
        assert!(!c.floor_active());
        assert_eq!(c.health_level(), Level::Info);
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_stale_sample_counts_as_a_lost_sensor() {
        let hw = FakeHwmon::new("stale");
        let mut c = two_sensor_controller(&hw);
        c.last_pwm = Some(100);
        c.set_samples(&[Some(40.0), Some(40.0)]);
        c.update();
        assert!(pwm_written(&hw) < 100);
        // The drive's reader wedged: its last sample ages out.
        c.feeds[1].max_age = Duration::from_millis(30);
        c.set_samples(&[Some(40.0), None]);
        *lock(&c.feeds[1].shared) = Sample {
            at: Instant::now().checked_sub(Duration::from_millis(60)),
            value: Some(40.0),
        };
        c.update();
        assert_eq!(pwm_written(&hw), 255);
        assert_eq!(c.health_level(), Level::Warn);
    }

    #[test]
    fn losing_one_of_several_sensors_holds_max_pwm_until_it_reads_steadily_again() {
        let hw = FakeHwmon::new("partial");
        let mut c = two_sensor_controller(&hw);
        c.last_pwm = Some(100);
        c.set_samples(&[Some(40.0), Some(40.0)]);
        c.update();
        let quiet = pwm_written(&hw);
        assert!(quiet < 100, "got {quiet}");
        assert_eq!(c.health_level(), Level::Info);

        // The drive sensor (which had been reading) goes silent while the
        // CPU still reads: the fan must not just follow the CPU.
        c.set_samples(&[Some(40.0), None]);
        c.update();
        assert_eq!(pwm_written(&hw), 255);
        assert!(c.floor_active());
        assert_eq!(c.health_level(), Level::Warn);

        // A reading comes back, but one reading isn't "recovered".
        c.set_samples(&[Some(40.0), Some(40.0)]);
        c.update();
        assert_eq!(pwm_written(&hw), 255);
        // Lost again inside the hold: stays held, no flapping to the curve.
        c.set_samples(&[Some(40.0), None]);
        c.update();
        c.set_samples(&[Some(40.0), Some(40.0)]);
        c.update();
        assert_eq!(pwm_written(&hw), 255);
        // Steady for the whole hold: back to the curve.
        std::thread::sleep(Duration::from_millis(100));
        c.update();
        assert_eq!(pwm_written(&hw), quiet);
        assert!(!c.floor_active());
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn a_selector_that_never_read_does_not_hold_the_fan() {
        let hw = FakeHwmon::new("absent");
        let mut c = two_sensor_controller(&hw);
        c.last_pwm = Some(100);
        for _ in 0..3 {
            c.set_samples(&[Some(40.0), None]); // no NVMe/drive installed
            c.update();
        }
        assert!(pwm_written(&hw) < 100);
        assert!(!c.floor_active());
    }

    #[test]
    fn reader_threads_publish_what_the_fixture_hwmon_reads() {
        let tree = crate::hal::HwmonTree::new("fan-reader");
        tree.chip("hwmon0", "coretemp", &[("temp1_input", "61000")]);
        let mut c = new_controller(FanProfile {
            sensors: vec![SensorSelector {
                chip: "coretemp".into(),
                ..SensorSelector::default()
            }],
            ..cfg()
        });
        c.hwmon_root = tree.0.clone();
        let stop = Arc::new(AtomicBool::new(false));
        c.start_readers(&stop);
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.feeds[0].current().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(c.feeds[0].current(), Some(61.0));
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn sampling_interval_is_clamped_to_a_safe_range() {
        let sel = |secs| SensorSelector {
            min_resample_secs: secs,
            ..SensorSelector::default()
        };
        assert_eq!(sampling_secs(&sel(None), 1), 1);
        assert_eq!(sampling_secs(&sel(Some(0)), 1), 1);
        assert_eq!(sampling_secs(&sel(Some(30)), 1), 30);
        assert_eq!(
            sampling_secs(&sel(Some(u64::MAX)), 1),
            MAX_SENSOR_INTERVAL_SECS
        );
        // Even an absurd update_secs can't overflow the staleness limit.
        let feed = SensorFeed::new(&sel(Some(u64::MAX)), u64::MAX);
        assert_eq!(
            feed.max_age,
            Duration::from_secs(MAX_SENSOR_INTERVAL_SECS * 3)
        );
    }

    /// Timing that makes "never took over" and "no sensor ever" react at
    /// once, with CRITICAL a moment later.
    fn impatient() -> Timing {
        Timing {
            control_warn_after: Duration::ZERO,
            control_critical_after: Duration::from_millis(80),
            sensor_takeover_after: Duration::ZERO,
            ..Timing::default()
        }
    }

    fn it8625_tree(tag: &str) -> crate::hal::HwmonTree {
        let tree = crate::hal::HwmonTree::new(tag);
        tree.chip("hwmon0", "it8625", &[("pwm1", "120"), ("pwm1_enable", "2")]);
        tree
    }

    fn coretemp_fan() -> FanProfile {
        FanProfile {
            fan_index: None,
            sensors: vec![SensorSelector {
                chip: "coretemp".into(),
                ..SensorSelector::default()
            }],
            ..cfg()
        }
    }

    #[test]
    fn a_pwm_chip_that_never_resolves_raises_warn_then_critical() {
        let empty = crate::hal::HwmonTree::new("no-chip");
        let mut c = new_controller(coretemp_fan());
        c.hwmon_root = empty.0.clone();
        c.timing = Timing {
            control_warn_after: Duration::from_millis(40),
            ..impatient()
        };
        // Within the boot grace: quiet.
        c.update();
        assert_eq!(c.health_level(), Level::Info);
        std::thread::sleep(Duration::from_millis(50));
        c.update();
        assert_eq!(c.health_level(), Level::Warn);
        assert!(c.control_loss.as_ref().is_some_and(|l| l.warned));
        std::thread::sleep(Duration::from_millis(50));
        c.update();
        assert_eq!(c.health_level(), Level::Critical);
        assert!(c.control_loss.as_ref().is_some_and(|l| l.critical));
    }

    #[test]
    fn health_recovers_once_a_late_chip_is_taken_over() {
        let tree = crate::hal::HwmonTree::new("late-chip");
        let mut c = new_controller(coretemp_fan());
        c.hwmon_root = tree.0.clone();
        c.timing = impatient();
        c.update();
        assert_eq!(c.health_level(), Level::Warn);
        // The platform driver loads: chip appears, a sensor reads.
        let hw = tree.chip("hwmon0", "it8625", &[("pwm1", "120"), ("pwm1_enable", "2")]);
        c.set_samples(&[Some(60.0)]);
        c.last_tick = None;
        c.update();
        assert!(c.control_loss.is_none());
        assert_eq!(c.health_level(), Level::Info);
        assert_eq!(
            std::fs::read_to_string(format!("{hw}/pwm1_enable")).unwrap(),
            "1"
        );
    }

    #[test]
    fn failing_pwm_writes_count_as_not_under_control() {
        let tree = it8625_tree("write-fail");
        let mut c = new_controller(coretemp_fan());
        c.hwmon_root = tree.0.clone();
        c.timing = impatient();
        c.set_samples(&[Some(60.0)]);
        // pwm1_enable is a directory: every write to it fails.
        let enable = tree.0.join("hwmon0/pwm1_enable");
        std::fs::remove_file(&enable).unwrap();
        std::fs::create_dir(&enable).unwrap();
        c.update();
        assert!(c.hwmon.is_none());
        assert_eq!(c.health_level(), Level::Warn);
    }

    #[test]
    fn no_sensor_ever_reading_takes_the_fan_over_at_max_pwm_after_the_grace() {
        let tree = it8625_tree("never-sensor");
        let hw = tree.0.join("hwmon0");
        let mut c = new_controller(coretemp_fan());
        c.hwmon_root = tree.0.clone();
        // Inside the grace: BIOS mode is left alone (normal at boot).
        c.timing = Timing {
            sensor_takeover_after: Duration::from_secs(60),
            control_warn_after: Duration::from_secs(60),
            ..impatient()
        };
        c.update();
        assert_eq!(std::fs::read_to_string(hw.join("pwm1")).unwrap(), "120");
        assert_eq!(c.health_level(), Level::Info);
        // Past it: manual mode at full speed, health Warn...
        c.timing.sensor_takeover_after = Duration::ZERO;
        c.timing.control_warn_after = Duration::ZERO;
        c.update();
        assert_eq!(std::fs::read_to_string(hw.join("pwm1")).unwrap(), "255");
        assert_eq!(
            std::fs::read_to_string(hw.join("pwm1_enable")).unwrap(),
            "1"
        );
        assert!(c.sensors_lost);
        assert_eq!(c.health_level(), Level::Warn);
        // ...and Critical when it persists.
        std::thread::sleep(Duration::from_millis(100));
        c.update();
        assert_eq!(c.health_level(), Level::Critical);
        assert!(c.no_sensor_critical_logged);
        // A sensor finally reads: the curve takes over and health clears.
        c.set_samples(&[Some(40.0)]);
        c.update();
        assert!(!c.sensors_lost);
        assert_eq!(c.health_level(), Level::Info);
    }

    #[test]
    fn a_fan_disabled_by_config_is_flagged_not_silent() {
        let (cfg, diagnostics) = crate::config::Config::parse(
            "[[fans]]\nname = \"bad\"\npwm_chip = \"it8625\"\nmin_temp_c = 90.0\nmax_temp_c = 45.0\n",
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("may stop it")),
            "{diagnostics:?}"
        );
        let c = FanController::new(cfg.fans[0].clone(), &cfg.temperature);
        assert_eq!(c.health_level(), Level::Warn);
        let line = c.status_line();
        assert!(line.contains("disabled by config"), "{line}");
        assert!(line.contains("[Warn]"), "{line}");
    }

    #[test]
    fn a_fan_disabled_on_purpose_is_just_disabled() {
        let c = new_controller(FanProfile {
            enabled: false,
            ..cfg()
        });
        assert_eq!(c.health_level(), Level::Info);
        assert!(
            c.status_line().contains("disabled [Info]"),
            "{}",
            c.status_line()
        );
    }
}
