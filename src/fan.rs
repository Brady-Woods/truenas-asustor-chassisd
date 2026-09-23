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
//! Two ways this goes further than upstream fancontrol:
//! - **Multiple sensors per fan, not one.** `FanProfile::sensors` is a
//!   list; the control temp is the max across whichever of them currently
//!   read as connected (see `hal::read_temp_input`), so e.g. a hot drive
//!   ramps the fan even with the CPU idle.
//! - **Multiple fans, not one.** `Config::fans` is a `Vec`; `main.rs` runs
//!   one `FanController` per entry, independently.
//!
//! `lcm-status fan-profile` (`fan_calibrate.rs`) is the discovery
//! counterpart to `pwmconfig`: it finds which pwm outputs and sensors
//! actually exist and what a fan's real min-start/min-stop PWM is, rather
//! than this module guessing.

use crate::config::FanProfile;
use crate::hal::{glob_hwmon, read_sysfs_raw_f32, resolve_selector};
use std::time::{Duration, Instant};

pub struct FanController {
    profile: FanProfile,
    hwmon: Option<String>,
    last_pwm: Option<u8>,
    /// Set while a stalled/stopped fan is being kicked with min_start_pwm;
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
}

impl FanController {
    pub fn new(profile: FanProfile) -> Self {
        let sensor_cache = vec![(None, None); profile.sensors.len()];
        FanController { profile, hwmon: None, last_pwm: None, kick_until: None, sensor_cache, last_tick: None }
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
            self.hwmon = glob_hwmon(&self.profile.pwm_chip).and_then(|v| v.into_iter().next());
        }
        let Some(hwmon) = self.hwmon.clone() else {
            return; // chip not loaded (yet) -- try again next tick
        };

        let Some(control_temp) = self.control_temp() else {
            return; // no connected sensor has a reading yet
        };

        // Mid-kick: leave min_start_pwm in place until it's had time to
        // actually get the fan spinning, then fall through to a normal
        // computation on the next call once it elapses.
        if let Some(until) = self.kick_until {
            if Instant::now() < until {
                return;
            }
            self.kick_until = None;
        }

        let current_pwm = self.last_pwm.unwrap_or(0);
        let current_rpm = self
            .profile
            .fan_index
            .and_then(|n| read_sysfs_raw_f32(&format!("{hwmon}/fan{n}_input")));
        // Stalled if we commanded it off, or a tachometer reading exists
        // and genuinely reads zero (not "reading unavailable"/"no tach
        // configured" -- neither of those is evidence of a stall).
        let stalled = current_pwm == 0 || matches!(current_rpm, Some(rpm) if rpm <= 0.0);

        if stalled {
            if self.write_pwm(&hwmon, self.profile.min_start_pwm).is_ok() {
                self.last_pwm = Some(self.profile.min_start_pwm);
                self.kick_until = Some(Instant::now() + Duration::from_secs(1));
            } else {
                self.hwmon = None; // path went stale -- re-resolve next tick
            }
            return;
        }

        let target = compute_pwm(control_temp, &self.profile);
        // Re-assert manual mode every tick, not just once: some
        // firmware/hardware resets pwmN_enable back to automatic on its
        // own, and fancontrol(8) itself defends against that the same way.
        if self.ensure_manual_mode(&hwmon).is_err() || self.write_pwm(&hwmon, target).is_err() {
            self.hwmon = None;
            return;
        }
        self.last_pwm = Some(target);
    }

    /// Max reading across every configured sensor that currently resolves
    /// to a connected value, each resampled on its own cadence (fast for
    /// CPU, which can spike quickly; slower for drive/NVMe temps, which
    /// change slowly and don't need hammering).
    fn control_temp(&mut self) -> Option<f32> {
        let mut max: Option<f32> = None;
        for i in 0..self.profile.sensors.len() {
            let min_resample = self.profile.sensors[i]
                .min_resample_secs
                .unwrap_or(self.profile.update_secs)
                .max(1);
            let need_refresh = match self.sensor_cache[i].0 {
                None => true,
                Some(t) => t.elapsed() >= Duration::from_secs(min_resample),
            };
            if need_refresh {
                let v = resolve_selector(&self.profile.sensors[i])
                    .into_iter()
                    .fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x))));
                self.sensor_cache[i] = (Some(Instant::now()), v);
            }
            if let Some(v) = self.sensor_cache[i].1 {
                max = Some(max.map_or(v, |m: f32| m.max(v)));
            }
        }
        max
    }

    fn ensure_manual_mode(&self, hwmon: &str) -> std::io::Result<()> {
        std::fs::write(format!("{hwmon}/pwm{}_enable", self.profile.pwm_index), "1")
    }

    fn write_pwm(&self, hwmon: &str, value: u8) -> std::io::Result<()> {
        std::fs::write(format!("{hwmon}/pwm{}", self.profile.pwm_index), value.to_string())
    }
}

/// Ported from lm-sensors' `fancontrol(8)` `UpdateFanSpeeds`: flat
/// `min_pwm` at/below `min_temp_c`, flat `max_pwm` at/above `max_temp_c`,
/// and in between a straight line from `min_stop_pwm` (not `min_pwm`) at
/// `min_temp_c` up to `max_pwm` at `max_temp_c`. Doesn't handle the
/// stall/kick case -- that's `FanController::update`'s job, since it needs
/// mutable state (`kick_until`) this pure function doesn't have.
fn compute_pwm(temp_c: f32, cfg: &FanProfile) -> u8 {
    let raw = if temp_c <= cfg.min_temp_c {
        cfg.min_pwm as f32
    } else if temp_c >= cfg.max_temp_c {
        cfg.max_pwm as f32
    } else {
        let (min_t, max_t) = (cfg.min_temp_c, cfg.max_temp_c);
        let (min_stop, max_pwm) = (cfg.min_stop_pwm as f32, cfg.max_pwm as f32);
        (temp_c - min_t) * (max_pwm - min_stop) / (max_t - min_t) + min_stop
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
        }
    }

    #[test]
    fn at_or_below_min_temp_is_flat_min_pwm() {
        assert_eq!(compute_pwm(30.0, &cfg()), 50);
        assert_eq!(compute_pwm(45.0, &cfg()), 50);
    }

    #[test]
    fn at_or_above_max_temp_is_flat_max_pwm() {
        assert_eq!(compute_pwm(90.0, &cfg()), 255);
        assert_eq!(compute_pwm(120.0, &cfg()), 255);
    }

    #[test]
    fn just_above_min_temp_starts_near_min_stop_not_min_pwm() {
        // The ramp's intercept at min_temp_c is min_stop_pwm(55), not
        // min_pwm(50) -- this is the non-obvious bit the graph in
        // fancontrol.txt documents.
        let pwm = compute_pwm(45.01, &cfg());
        assert!((54..=56).contains(&pwm), "got {pwm}");
    }

    #[test]
    fn midpoint_interpolates_from_min_stop_to_max_pwm() {
        // halfway between 45 and 90 -> halfway between min_stop(55) and 255
        let pwm = compute_pwm(67.5, &cfg());
        let expected = 55 + (255 - 55) / 2;
        assert!((pwm as i32 - expected as i32).abs() <= 1, "got {pwm}");
    }

    #[test]
    fn matches_the_actual_curve_used_before_this_replaced_fancontrol() {
        // Spot-check against values observed live (via /etc/fancontrol,
        // the same curve) on nas.skycorgi.net before this daemon took
        // over: modest CPU load kept pwm1 in the 100-200 range.
        let pwm = compute_pwm(60.0, &cfg());
        assert!((100..=170).contains(&pwm), "got {pwm}");
    }

    #[test]
    fn matches_live_observed_value_at_67c() {
        // CPU package temp 67C observed live on nas.skycorgi.net produced
        // pwm1=153 through this exact formula -- pinned here so a future
        // change to the algorithm has to justify moving this number.
        assert_eq!(compute_pwm(67.0, &cfg()), 153);
    }
}
