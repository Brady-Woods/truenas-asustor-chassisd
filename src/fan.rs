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
//! `lcm-status fan-profile` (`fan_calibrate.rs`) is the discovery
//! counterpart to `pwmconfig`: it finds which pwm outputs and sensors
//! actually exist and what a fan's real min-start/min-stop PWM is, rather
//! than this module guessing.

use crate::config::FanProfile;
use crate::hal::{glob_hwmon, read_sysfs_raw_f32, resolve_selector};
use crate::socket::Level;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How often the fan thread wakes to give each controller a chance to
/// tick. Each controller still only acts every `update_secs`.
const FAN_THREAD_INTERVAL: Duration = Duration::from_millis(100);

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
    pub fn spawn(profiles: Vec<FanProfile>) -> std::io::Result<Self> {
        let status = Arc::new(Mutex::new(FanStatus {
            health: Level::Info,
            lines: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new().name("fans".into()).spawn({
            let status = Arc::clone(&status);
            let stop = Arc::clone(&stop);
            move || {
                let mut fans: Vec<FanController> =
                    profiles.into_iter().map(FanController::new).collect();
                while !stop.load(Ordering::Relaxed) {
                    for f in &mut fans {
                        f.tick();
                    }
                    let snapshot = FanStatus {
                        health: worst_health(&fans),
                        lines: fans.iter().map(FanController::status_line).collect(),
                    };
                    *status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
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
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
    /// Parallel to `profile.sensors`: (last refresh time, last resolved
    /// max value) per selector, each on its own `min_resample_secs`.
    sensor_cache: Vec<(Option<Instant>, Option<f32>)>,
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
    /// `pwmN_enable` as found before this controller first took the fan
    /// over, restored on exit (see `restore`).
    original_enable: Option<String>,
}

impl FanController {
    /// One line for the `status` report: name, commanded pwm (raw + %),
    /// live RPM (fresh sysfs read, not cached), and current health. `--`
    /// for RPM if this profile has no `fan_index` configured (no tach to
    /// read) or the chip isn't resolved (not loaded yet).
    pub fn status_line(&self) -> String {
        let pwm = self.last_pwm.unwrap_or(0);
        let pct = pwm as u32 * 100 / 255;
        let rpm = match (&self.hwmon, self.profile.fan_index) {
            (Some(hwmon), Some(n)) => read_sysfs_raw_f32(&format!("{hwmon}/fan{n}_input"))
                .map(|r| format!("{r:.0}rpm"))
                .unwrap_or_else(|| "--".to_string()),
            _ => "--".to_string(),
        };
        format!(
            "{} (pwm{}): pwm={pwm} ({pct}%) {rpm} [{:?}]",
            self.profile.name,
            self.profile.pwm_index,
            self.health_level()
        )
    }

    /// Current health, for the status LED (`state::recompute_status_led`)
    /// -- current state, not transition-gated the way this fan's own
    /// syslog lines are (the LED always reflects "right now").
    pub fn health_level(&self) -> Level {
        if self.consecutive_stalls >= UNRESPONSIVE_AFTER_STALLS {
            Level::Critical
        } else if self.consecutive_stalls > 0 || self.low_rpm_warned || self.sensors_lost {
            Level::Warn
        } else {
            Level::Info
        }
    }

    pub fn new(profile: FanProfile) -> Self {
        let sensor_cache = vec![(None, None); profile.sensors.len()];
        FanController {
            profile,
            hwmon: None,
            last_pwm: None,
            kick_until: None,
            sensor_cache,
            last_tick: None,
            ever_ran_normally: false,
            consecutive_stalls: 0,
            low_rpm_warned: false,
            sensors_lost: false,
            original_enable: None,
        }
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
            return; // chip not loaded (yet) -- try again next tick
        };
        if self.original_enable.is_none() {
            self.original_enable = std::fs::read_to_string(self.enable_path(&hwmon))
                .ok()
                .map(|s| s.trim().to_string());
        }

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

        match self.plan(sensor_target, rpm) {
            PwmCommand::Kick(pwm) => {
                if self.write_pwm(&hwmon, pwm).is_ok() {
                    self.last_pwm = Some(pwm);
                    self.kick_until = Some(Instant::now() + Duration::from_secs(1));
                } else {
                    self.hwmon = None; // path went stale -- re-resolve next tick
                }
            }
            PwmCommand::Set(pwm) => {
                // Re-assert manual mode every tick, not just once: some
                // firmware/hardware resets pwmN_enable back to automatic
                // on its own, and fancontrol(8) defends against that the
                // same way.
                if self.ensure_manual_mode(&hwmon).is_err() || self.write_pwm(&hwmon, pwm).is_err()
                {
                    self.hwmon = None;
                    return;
                }
                self.last_pwm = Some(pwm);
            }
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
            let low = rpm < min_rpm as f32;
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

    /// The PWM this fan should run at, right now, per its *hottest-demanding*
    /// sensor -- not simply "feed the hottest raw temperature through one
    /// shared curve". Each sensor selector can define its own
    /// `min_temp_c`/`max_temp_c` (falling back to the fan profile's own if
    /// unset), so e.g. a drive can be configured to hit `max_pwm` at its
    /// own critical threshold (60C) even though the fan's CPU-tuned curve
    /// doesn't reach `max_temp_c` until 90C. Evaluating each sensor against
    /// its own curve and taking the worst *resulting PWM* (not the worst
    /// raw temperature first) is what makes that actually work: a drive at
    /// 61C against a 45-90C shared curve would only compute to a modest
    /// partial speed, nowhere near the full-speed response its own 60C
    /// critical threshold warrants.
    ///
    /// Each sensor is resampled on its own cadence (fast for CPU, which
    /// can spike quickly; slower for drive/NVMe temps, which change slowly
    /// and don't need hammering).
    fn target_pwm_from_sensors(&mut self) -> Option<u8> {
        let mut target: Option<u8> = None;
        for i in 0..self.profile.sensors.len() {
            let sel = &self.profile.sensors[i];
            let min_resample = sel
                .min_resample_secs
                .unwrap_or(self.profile.update_secs)
                .max(1);
            let need_refresh = match self.sensor_cache[i].0 {
                None => true,
                Some(t) => t.elapsed() >= Duration::from_secs(min_resample),
            };
            if need_refresh {
                let v = resolve_selector(sel)
                    .into_iter()
                    .fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x))));
                self.sensor_cache[i] = (Some(Instant::now()), v);
            }
            if let Some(temp_c) = self.sensor_cache[i].1 {
                let min_t = sel.min_temp_c.unwrap_or(self.profile.min_temp_c);
                let max_t = sel.max_temp_c.unwrap_or(self.profile.max_temp_c);
                let pwm = compute_pwm(temp_c, min_t, max_t, &self.profile);
                target = Some(target.map_or(pwm, |m: u8| m.max(pwm)));
            }
        }
        target
    }

    /// No sensor feeding this fan has a reading. Before this controller
    /// has ever written the fan (e.g. sensors not loaded yet at startup)
    /// it's left alone, in whatever mode the BIOS/driver set. Once it has
    /// taken the fan over, losing every sensor (a module unloaded, a chip
    /// renumbered) would otherwise freeze the fan at its last speed with
    /// nothing watching temperatures -- so fail safe to `max_pwm` instead.
    fn hold_full_speed_without_sensors(&mut self, hwmon: &str) {
        if self.last_pwm.is_none() {
            return;
        }
        if !self.sensors_lost {
            self.sensors_lost = true;
            crate::syslog::warning(&format!(
                "fan '{}' (pwm{}): no sensor readings, holding at max_pwm",
                self.profile.name, self.profile.pwm_index
            ));
        }
        self.kick_until = None;
        let max = self.profile.max_pwm;
        if self.ensure_manual_mode(hwmon).is_err() || self.write_pwm(hwmon, max).is_err() {
            self.hwmon = None;
            return;
        }
        self.last_pwm = Some(max);
    }

    /// Hands the fan back on exit (clean shutdown, or unwinding from a
    /// panic -- this runs from `Drop`), so it never sits at a stale manual
    /// speed with nothing controlling it. Restores the original
    /// `pwmN_enable` mode if that was automatic; if it was already manual
    /// (e.g. left that way by a previous crash) or unknown, leaves it at
    /// full speed instead, the same fail-safe fancontrol(8) uses.
    fn restore(&mut self) {
        if self.last_pwm.is_none() {
            return; // never took control
        }
        let Some(hwmon) = self.hwmon.clone().or_else(|| self.resolve_hwmon()) else {
            return;
        };
        if let Some(mode) = self.original_enable.as_deref().filter(|m| *m != "1") {
            if std::fs::write(self.enable_path(&hwmon), mode).is_ok() {
                crate::syslog::info(&format!(
                    "fan '{}' (pwm{}): restored pwm{}_enable={mode}",
                    self.profile.name, self.profile.pwm_index, self.profile.pwm_index
                ));
                return;
            }
        }
        if self.write_pwm(&hwmon, u8::MAX).is_ok() {
            crate::syslog::info(&format!(
                "fan '{}' (pwm{}): left at full speed on exit",
                self.profile.name, self.profile.pwm_index
            ));
        }
    }

    fn resolve_hwmon(&self) -> Option<String> {
        glob_hwmon(&self.profile.pwm_chip).and_then(|v| v.into_iter().next())
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
        cfg.min_pwm as f32
    } else if temp_c >= max_temp_c {
        cfg.max_pwm as f32
    } else {
        let (min_stop, max_pwm) = (cfg.min_stop_pwm as f32, cfg.max_pwm as f32);
        (temp_c - min_temp_c) * (max_pwm - min_stop) / (max_temp_c - min_temp_c) + min_stop
    };
    raw.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FanProfile {
        FanProfile {
            name: "test".to_string(),
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
            sensors: Vec::new(),
            min_expected_rpm: None,
        }
    }

    /// Evaluates against `cfg()`'s own min_temp_c/max_temp_c (45/90) --
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
        assert!((p as i32 - expected as i32).abs() <= 1, "got {p}");
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
        let mut c = FanController::new(FanProfile {
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
    fn drop_restores_original_automatic_mode() {
        let hw = FakeHwmon::new("restore-auto");
        let mut c = controller_on(&hw);
        c.update(); // captures original_enable = "2"
        c.last_pwm = Some(100);
        std::fs::write(hw.0.join("pwm1_enable"), "1").unwrap();
        drop(c);
        assert_eq!(hw.read("pwm1_enable"), "2");
    }

    #[test]
    fn drop_leaves_fan_at_full_speed_if_it_was_already_manual() {
        let hw = FakeHwmon::new("restore-manual");
        std::fs::write(hw.0.join("pwm1_enable"), "1").unwrap();
        let mut c = controller_on(&hw);
        c.update(); // captures original_enable = "1"
        c.last_pwm = Some(100);
        drop(c);
        assert_eq!(hw.read("pwm1"), "255");
    }

    fn plan_controller() -> FanController {
        FanController::new(cfg())
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
}
