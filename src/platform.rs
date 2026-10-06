//! The board's power-related BIOS settings, as the platform driver
//! (`adm-parity` branch) exposes them under `/sys/devices/platform/asustor`:
//! `ac_power_resume` (`off`/`last`/`on`, what happens when power returns
//! after a loss) and `eup` (`0`/`1`, ErP/EuP deep power saving).
//!
//! **Read-only.** The daemon never writes these: the driver only accepts
//! writes when loaded with `allow_power_config=1`, and changing BIOS power
//! behaviour is not something to do behind the user's back. They are
//! read to warn when EuP would defeat a configured wake source (it cuts
//! standby power in soft-off, so neither Wake-on-LAN nor the RTC alarm
//! can power the box back on), and to show both in `lcm-status status`.

use crate::config::Config;
use std::path::Path;

/// The `asustor` platform device. Exists whenever the module is loaded on
/// a supported board, so it's also how the driver is detected (see
/// `led::driver_present`).
pub const PLATFORM_DIR: &str = "/sys/devices/platform/asustor";

/// The LCD power rail, as `lcm-status status` shows it: the driver's
/// `lcd_power` (`1`/`0`), which it switches on and holds. **Never
/// written here** -- `0` cuts the whole LCD module, its MCU and so the
/// front-panel buttons included; night mode uses the panel's own
/// display-off command instead.
pub fn lcd_power() -> String {
    lcd_power_in(Path::new(PLATFORM_DIR))
}

fn lcd_power_in(dir: &Path) -> String {
    if !dir.is_dir() {
        return "unknown (platform driver not loaded)".into();
    }
    match std::fs::read_to_string(dir.join("lcd_power")) {
        Ok(v) => match v.trim() {
            "1" => "on (lcd_power = 1, held by the driver)".into(),
            "0" => "OFF (lcd_power = 0: the panel and its buttons are dead)".into(),
            other => format!("lcd_power = {other}"),
        },
        Err(_) => "not exposed by this driver (no lcd_power)".into(),
    }
}

/// What the driver reports; `None` where a file doesn't exist (an older
/// driver, or a board it doesn't know these for).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PowerSettings {
    pub ac_power_resume: Option<String>,
    pub eup: Option<String>,
}

impl PowerSettings {
    pub fn read() -> Self {
        PowerSettings::read_from(Path::new(PLATFORM_DIR))
    }

    fn read_from(dir: &Path) -> Self {
        let attr = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .map(|s| s.trim().to_string())
        };
        PowerSettings {
            ac_power_resume: attr("ac_power_resume"),
            eup: attr("eup"),
        }
    }

    fn eup_on(&self) -> bool {
        self.eup.as_deref() == Some("1")
    }

    /// Report lines; empty when the driver exposes neither file.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(resume) = &self.ac_power_resume {
            let meaning = match resume.as_str() {
                "off" => " (stay off when power returns)",
                "last" => " (return to the state before the power loss)",
                "on" => " (power on when power returns)",
                _ => "",
            };
            out.push(format!("AC power resume: {resume}{meaning}"));
        }
        if let Some(eup) = &self.eup {
            let meaning = match eup.as_str() {
                "0" => " (off)",
                "1" => " (on: no standby power in soft-off)",
                _ => "",
            };
            out.push(format!("EuP:             {eup}{meaning}"));
        }
        out
    }
}

/// What in `cfg` relies on waking the box from soft-off: Wake-on-LAN
/// kept enabled on some NIC, and/or a `power_on` schedule rule.
pub fn wake_sources(cfg: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    if !cfg.wol.nics.is_empty() && crate::wol::parse_mode(&cfg.wol.mode).is_ok_and(|b| b != 0) {
        out.push("Wake-on-LAN");
    }
    if crate::power::parse_rules(&cfg.power_schedule)
        .iter()
        .any(|r| r.action == crate::power::PowerAction::PowerOn)
    {
        out.push("the power schedule's RTC wake");
    }
    out
}

/// The warning to give when EuP is on and something configured needs it
/// off; `None` otherwise.
pub fn eup_warning(settings: &PowerSettings, wake: &[&str]) -> Option<String> {
    (settings.eup_on() && !wake.is_empty()).then(|| {
        format!(
            "EuP is on ({PLATFORM_DIR}/eup = 1): the board cuts standby power in soft-off, \
             so {} may not wake it; turn ErP/EuP off in the BIOS",
            wake.join(" and ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eup(value: &str) -> PowerSettings {
        PowerSettings {
            ac_power_resume: None,
            eup: Some(value.to_string()),
        }
    }

    #[test]
    fn missing_files_read_as_none_and_describe_nothing() {
        let settings = PowerSettings::read_from(Path::new("/nonexistent"));
        assert_eq!(settings, PowerSettings::default());
        assert_eq!(settings.describe(), Vec::<String>::new());
    }

    #[test]
    fn files_are_read_trimmed() {
        let dir = std::env::temp_dir().join(format!("lcm-status-platform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ac_power_resume"), "last\n").unwrap();
        std::fs::write(dir.join("eup"), "0\n").unwrap();
        let settings = PowerSettings::read_from(&dir);
        assert_eq!(settings.ac_power_resume.as_deref(), Some("last"));
        assert_eq!(settings.eup.as_deref(), Some("0"));
        let lines = settings.describe();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("AC power resume: last"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lcd_power_is_read_not_written() {
        assert!(lcd_power_in(Path::new("/nonexistent")).contains("not loaded"));
        let dir = std::env::temp_dir().join(format!("lcm-status-lcd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(lcd_power_in(&dir).contains("not exposed"));
        std::fs::write(dir.join("lcd_power"), "1\n").unwrap();
        assert!(lcd_power_in(&dir).starts_with("on"));
        std::fs::write(dir.join("lcd_power"), "0\n").unwrap();
        assert!(lcd_power_in(&dir).starts_with("OFF"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wake_sources_come_from_wol_and_power_on_rules() {
        let (cfg, _) = Config::parse("");
        assert_eq!(wake_sources(&cfg), Vec::<&str>::new());

        let (cfg, _) = Config::parse("[wol]\nnics = [\"enp2s0\"]\n");
        assert_eq!(wake_sources(&cfg), ["Wake-on-LAN"]);
        // WOL kept *off* isn't a wake source.
        let (cfg, _) = Config::parse("[wol]\nnics = [\"enp2s0\"]\nmode = \"d\"\n");
        assert_eq!(wake_sources(&cfg), Vec::<&str>::new());

        let rule = |action: &str| {
            format!(
                "[[power_schedule]]\ndays = \"daily\"\ntime = \"07:00\"\naction = \"{action}\"\n"
            )
        };
        let (cfg, _) = Config::parse(&rule("shutdown"));
        assert_eq!(wake_sources(&cfg), Vec::<&str>::new());
        let (cfg, _) = Config::parse(&rule("power_on"));
        assert_eq!(wake_sources(&cfg), ["the power schedule's RTC wake"]);
        let (cfg, _) = Config::parse(&(rule("power_on") + "enabled = false\n"));
        assert_eq!(wake_sources(&cfg), Vec::<&str>::new());
    }

    #[test]
    fn eup_warns_only_when_on_and_something_needs_wake() {
        assert!(eup_warning(&eup("1"), &["Wake-on-LAN"]).is_some());
        assert!(eup_warning(&eup("1"), &[]).is_none());
        assert!(eup_warning(&eup("0"), &["Wake-on-LAN"]).is_none());
        assert!(eup_warning(&PowerSettings::default(), &["Wake-on-LAN"]).is_none());
        let both = eup_warning(&eup("1"), &["Wake-on-LAN", "the RTC"]).unwrap();
        assert!(both.contains("Wake-on-LAN and the RTC"));
    }
}
