//! Alarms: the conditions that change the front status LED, each with the
//! message the LCD shows for it. Whatever the most critical active alarm
//! is decides both -- the LED's colour/pattern and the LCD text, which
//! replaces the rotating status screens while it lasts (`state.rs`).
//!
//! Built from two sources, so the LED and the LCD can't disagree: the
//! daemon's own health checks (`AppState::health_summary`) and an
//! error/critical message pushed over the socket (`SHOW`).

use crate::led::StatusPattern;
use crate::socket::Level;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    pub level: Level,
    /// What the status LED shows while this is the most critical alarm.
    pub pattern: StatusPattern,
    pub line0: String,
    pub line1: String,
}

impl Alarm {
    /// `None` for `Level::Info`, which is never an alarm.
    pub fn new(level: Level, line0: &str, line1: &str) -> Option<Alarm> {
        Some(Alarm {
            level,
            pattern: health_pattern(level)?,
            line0: line0.to_string(),
            line1: line1.to_string(),
        })
    }

    /// An alarm of a fixed pattern whose level doesn't map straight onto it
    /// (a degraded pool is a warning, but not the generic amber one).
    pub fn with_pattern(level: Level, pattern: StatusPattern, line0: &str, line1: &str) -> Alarm {
        Alarm {
            level,
            pattern,
            line0: line0.to_string(),
            line1: line1.to_string(),
        }
    }
}

/// The status LED pattern a health `Level` shows, `None` for `Info`: amber
/// for a warning, solid red for an error, flashing red for critical.
pub fn health_pattern(level: Level) -> Option<StatusPattern> {
    match level {
        Level::Info => None,
        Level::Warn => Some(StatusPattern::Warning),
        Level::Error => Some(StatusPattern::Failed),
        Level::Critical => Some(StatusPattern::CriticalFlashing),
    }
}

/// The alarm to show: the most severe by LED pattern, the earliest of any
/// equally severe ones -- so callers list them in the order they want ties
/// broken.
pub fn most_critical(alarms: impl IntoIterator<Item = Alarm>) -> Option<Alarm> {
    alarms.into_iter().fold(None, |best, a| match best {
        Some(b) if b.pattern.severity() >= a.pattern.severity() => Some(b),
        _ => Some(a),
    })
}

/// A short name for a hwmon chip, for the 16-character LCD.
pub fn short_chip_name(chip: &str) -> &str {
    match chip {
        "coretemp" => "CPU",
        "drivetemp" => "HDD",
        "nvme" => "NVMe",
        "it8625" => "SYSTEM",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(level: Level, text: &str) -> Alarm {
        Alarm::new(level, text, "").unwrap()
    }

    #[test]
    fn info_is_not_an_alarm() {
        assert_eq!(Alarm::new(Level::Info, "x", "y"), None);
    }

    #[test]
    fn the_most_severe_alarm_wins() {
        let top = most_critical([
            alarm(Level::Warn, "warm"),
            alarm(Level::Critical, "fire"),
            alarm(Level::Error, "disk"),
        ]);
        assert_eq!(top.unwrap().line0, "fire");
        assert_eq!(most_critical([]), None);
    }

    #[test]
    fn ties_go_to_the_earliest() {
        let top = most_critical([alarm(Level::Error, "first"), alarm(Level::Error, "second")]);
        assert_eq!(top.unwrap().line0, "first");
    }

    #[test]
    fn a_degraded_pool_outranks_a_warning_but_not_an_error() {
        let degraded =
            Alarm::with_pattern(Level::Warn, StatusPattern::Degraded, "pool", "DEGRADED");
        let top = most_critical([alarm(Level::Warn, "warm"), degraded.clone()]);
        assert_eq!(top, Some(degraded.clone()));
        let top = most_critical([degraded, alarm(Level::Error, "disk")]);
        assert_eq!(top.unwrap().line0, "disk");
    }

    #[test]
    fn chip_names_fit_the_panel() {
        assert_eq!(short_chip_name("coretemp"), "CPU");
        assert_eq!(short_chip_name("enp9s0"), "enp9s0");
    }
}
