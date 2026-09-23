//! Health monitoring: logs to syslog (see `syslog.rs`) on state
//! transitions -- temperature thresholds, ZFS pool health, and drive SMART
//! status. Hooked into `state.rs`'s existing refresh cadence
//! (`pools_min_secs`/`hdd_min_secs`/`temperature_min_secs`) rather than
//! polling independently, so this adds no new `zpool`/`smartctl` calls
//! beyond what the display screens already make -- `check_pools`/
//! `check_bays` take data the caller already fetched for its own screen.
//!
//! Every check here is transition-gated: logs once when a condition is
//! entered, once when it clears, never every poll for a sustained
//! condition. A syslog flooded with a repeat of the same line every 10s
//! is worse than useless for actually noticing when something changes --
//! the *current* state is always still visible through the daemon's own
//! screens/`hal-test`, this is specifically for "something changed, right
//! now" events.

use crate::config::TemperatureConfig;
use crate::hal::all_connected_temps;
use crate::led::BayState;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempLevel {
    Normal,
    Warning,
    Critical,
}

pub struct HealthMonitor {
    temp_levels: HashMap<String, TempLevel>,
    pool_healths: HashMap<String, String>,
    bay_states: HashMap<u32, BayState>,
    /// Temp monitoring runs on its own clock (`maybe_check_temps`),
    /// deliberately not reusing `AppState`'s `temperature_cache` --
    /// that cache is gated on `cfg.screens.temperature` (a *display*
    /// preference), and alerting has no business being silently disabled
    /// because someone turned off an LCD screen.
    last_temp_check: Option<Instant>,
}

impl HealthMonitor {
    pub fn new() -> Self {
        HealthMonitor {
            temp_levels: HashMap::new(),
            pool_healths: HashMap::new(),
            bay_states: HashMap::new(),
            last_temp_check: None,
        }
    }

    /// Call every tick; a no-op except once every `temperature_min_secs`
    /// (reusing that floor for monitoring cadence even though, unlike the
    /// temperature *screen*, this never skips based on `cfg.screens.*`).
    pub fn maybe_check_temps(&mut self, cfg: &crate::config::Config) {
        let interval = Duration::from_secs(cfg.refresh.temperature_min_secs.max(1));
        let due = match self.last_temp_check {
            None => true,
            Some(t) => t.elapsed() >= interval,
        };
        if !due {
            return;
        }
        self.last_temp_check = Some(Instant::now());
        self.check_temps(&cfg.temperature);
    }

    /// Every currently-connected temp sensor on the box (not just what a
    /// fan curve happens to use) against `warn_threshold`/
    /// `critical_threshold`. A sensor that drops out of the connected set
    /// entirely (module reload, disk removed) just stops being tracked --
    /// not treated as a return to "normal", since it isn't a real reading.
    fn check_temps(&mut self, cfg: &TemperatureConfig) {
        for (label, temp_c) in all_connected_temps() {
            let level = if temp_c >= cfg.critical_threshold {
                TempLevel::Critical
            } else if temp_c >= cfg.warn_threshold {
                TempLevel::Warning
            } else {
                TempLevel::Normal
            };
            let prev = self.temp_levels.insert(label.clone(), level);
            if prev == Some(level) {
                continue; // no change
            }
            match level {
                TempLevel::Critical => crate::syslog::critical(&format!(
                    "{label}: {temp_c:.1}C, at or above critical threshold ({:.1}C)",
                    cfg.critical_threshold
                )),
                TempLevel::Warning => crate::syslog::warning(&format!(
                    "{label}: {temp_c:.1}C, at or above warning threshold ({:.1}C)",
                    cfg.warn_threshold
                )),
                TempLevel::Normal => {
                    if prev.is_some() {
                        crate::syslog::notice(&format!("{label}: back to {temp_c:.1}C, below warning threshold"));
                    }
                }
            }
        }
    }

    /// `healths`: (pool name, health string) from `zpool list -H -o
    /// name,health`, e.g. `[("NVMe", "ONLINE"), ("HDD", "DEGRADED")]` --
    /// pass in whatever `hal::pool_healths()` the caller already fetched
    /// for its own screen/LED refresh, not a fresh call.
    pub fn check_pools(&mut self, healths: &[(String, String)]) {
        for (name, health) in healths {
            let prev = self.pool_healths.insert(name.clone(), health.clone());
            if prev.as_deref() == Some(health.as_str()) {
                continue;
            }
            match health.as_str() {
                "ONLINE" => {
                    if let Some(p) = &prev {
                        crate::syslog::notice(&format!("pool {name}: back to ONLINE (was {p})"));
                    }
                }
                "DEGRADED" => crate::syslog::warning(&format!("pool {name}: DEGRADED")),
                // FAULTED, UNAVAIL, OFFLINE, REMOVED, or anything else
                // zpool might report that isn't the two states above --
                // all of these mean at least part of the pool isn't
                // serving data normally, so treat unrecognized values as
                // critical too rather than silently ignoring them.
                other => crate::syslog::critical(&format!("pool {name}: {other}")),
            }
        }
    }

    /// `states`: per-bay LED state as `hal::bay_led_states()`/
    /// `AppState::update_health_leds` already computed from SMART for this
    /// refresh cycle -- reused here rather than re-running `smartctl`.
    /// Only `Failed` is logged (a confirmed SMART failure); `Alert` is an
    /// external override, not itself a health signal, and `Standby` is
    /// routine.
    pub fn check_bays(&mut self, states: &[(u32, BayState)]) {
        for (bay, state) in states {
            let prev = self.bay_states.insert(*bay, *state);
            if prev == Some(*state) {
                continue;
            }
            match state {
                BayState::Failed => crate::syslog::critical(&format!("bay {bay}: SMART reports FAILED")),
                _ => {
                    if prev == Some(BayState::Failed) {
                        crate::syslog::notice(&format!("bay {bay}: SMART no longer reports FAILED"));
                    }
                }
            }
        }
    }
}
