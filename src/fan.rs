//! Fan control -- drives `pwm1` (the IT8625's one populated fan header,
//! `it8625` hwmon chip) from temperature.
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
//! is confusing on its own -- it was for us too, the first time around
//! (see git history: the first version of this file got it wrong and
//! shipped `min_pwm` as the ramp's intercept instead).
//!
//! Unlike upstream fancontrol, this also folds in drive/NVMe temps, not
//! just CPU -- the control temp is the max of whatever's currently
//! available, so a hot drive can ramp the fan even if the CPU is idle.

use crate::config::FanConfig;
use crate::hal::{coretemp_package, drive_and_nvme_temps, glob_hwmon, read_sysfs_raw_f32};
use std::time::{Duration, Instant};

pub struct FanState {
    hwmon: Option<String>,
    last_pwm: Option<u8>,
    /// Set while a stalled/stopped fan is being kicked with min_start_pwm;
    /// the real curve value is written once this elapses. Non-blocking by
    /// design -- upstream fancontrol just `sleep 1`s inline, which is fine
    /// for a single-purpose daemon but would freeze LCD/button handling
    /// here for a full second. Spread across normal tick cadence instead.
    kick_until: Option<Instant>,
    last_drive_temp_refresh: Option<Instant>,
    cached_max_drive_temp: Option<f32>,
}

impl FanState {
    pub fn new() -> Self {
        FanState {
            hwmon: None,
            last_pwm: None,
            kick_until: None,
            last_drive_temp_refresh: None,
            cached_max_drive_temp: None,
        }
    }

    /// Best-effort, like the rest of the hardware glue here: any failure
    /// (module not loaded yet, hwmon renumbered after a reload, a
    /// permission problem) is swallowed and retried next tick rather than
    /// taking the whole daemon down. Caller (main.rs) gates how often this
    /// runs via `cfg.update_secs`.
    pub fn update(&mut self, cfg: &FanConfig) {
        if !cfg.enabled {
            return;
        }

        if self.hwmon.is_none() {
            self.hwmon = glob_hwmon("it8625").and_then(|v| v.into_iter().next());
        }
        let Some(hwmon) = self.hwmon.clone() else {
            return; // asustor_it87 not loaded (yet) -- try again next tick
        };

        let Some(control_temp) = self.control_temp(cfg) else {
            return;
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
        let current_rpm = read_sysfs_raw_f32(&format!("{hwmon}/fan1_input"));
        // Stalled if we commanded it off, or a tachometer reading exists
        // and genuinely reads zero (not "reading unavailable" -- an
        // unreadable sensor isn't evidence of a stall).
        let stalled = current_pwm == 0 || matches!(current_rpm, Some(rpm) if rpm <= 0.0);

        if stalled {
            if write_pwm(&hwmon, cfg.min_start_pwm).is_ok() {
                self.last_pwm = Some(cfg.min_start_pwm);
                self.kick_until = Some(Instant::now() + Duration::from_secs(1));
            } else {
                self.hwmon = None; // path went stale -- re-resolve next tick
            }
            return;
        }

        let target = compute_pwm(control_temp, cfg);
        // Re-assert manual mode every tick, not just once: some
        // firmware/hardware resets pwmN_enable back to automatic on its
        // own, and fancontrol(8) itself defends against that the same way.
        if ensure_manual_mode(&hwmon).is_err() || write_pwm(&hwmon, target).is_err() {
            self.hwmon = None;
            return;
        }
        self.last_pwm = Some(target);
    }

    /// max(CPU package temp, hottest drive/NVMe temp) -- whichever sensor
    /// is hottest drives the curve, not CPU alone. CPU is read fresh every
    /// call (cheap, and CPU load can spike quickly); drive/NVMe temps are
    /// resampled only every `drive_temp_min_secs` (slow-changing, and
    /// deliberately not hammered -- see `hal::drive_and_nvme_temps`).
    fn control_temp(&mut self, cfg: &FanConfig) -> Option<f32> {
        let cpu = coretemp_package();

        let need_drive_refresh = match self.last_drive_temp_refresh {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_secs(cfg.drive_temp_min_secs.max(1)),
        };
        if need_drive_refresh {
            self.cached_max_drive_temp =
                drive_and_nvme_temps().into_iter().fold(None, |max, t| Some(max.map_or(t, |m: f32| m.max(t))));
            self.last_drive_temp_refresh = Some(Instant::now());
        }

        match (cpu, self.cached_max_drive_temp) {
            (Some(c), Some(d)) => Some(c.max(d)),
            (Some(c), None) => Some(c),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        }
    }
}

/// Ported from lm-sensors' `fancontrol(8)` `UpdateFanSpeeds`: flat
/// `min_pwm` at/below `min_temp_c`, flat `max_pwm` at/above `max_temp_c`,
/// and in between a straight line from `min_stop_pwm` (not `min_pwm`) at
/// `min_temp_c` up to `max_pwm` at `max_temp_c`. Doesn't handle the
/// stall/kick case -- that's `FanState::update`'s job, since it needs
/// mutable state (`kick_until`) this pure function doesn't have.
fn compute_pwm(temp_c: f32, cfg: &FanConfig) -> u8 {
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

fn ensure_manual_mode(hwmon: &str) -> std::io::Result<()> {
    std::fs::write(format!("{hwmon}/pwm1_enable"), "1")
}

fn write_pwm(hwmon: &str, value: u8) -> std::io::Result<()> {
    std::fs::write(format!("{hwmon}/pwm1"), value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FanConfig {
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
}
