//! Notices the platform driver (the asustor-platform-driver fork's
//! `asustor` module, and its vendored `it87`'s front LED dimmer) showing up
//! after the daemon started, or being reloaded, so the LED settings that
//! only take effect on its devices can be applied (again).
//!
//! At boot this is the normal order, not an edge case: lcm-status.service
//! is started by systemd, while the driver is loaded by its own TrueNAS
//! Post Init script (its modules aren't installed into the boot pool,
//! which TrueNAS updates replace). That runs once boot has finished and
//! can't be ordered against a systemd unit. Found live (2026-10-06): the
//! daemon started about a second before the modules were loaded, so the
//! startup LED pass (`AppState::init_leds`) found no `disk_led_ready` and
//! no `front_panel::brightness`, and nothing applied them until a manual
//! restart. The driver's deploy script also reloads the modules whenever
//! its checkout changes, which re-creates every LED in its default state.
//!
//! How: every `CHECK_INTERVAL` (from the event loop -- a few `stat`s), a
//! `Snapshot` of the inode numbers of the platform device's directory, the
//! LEDs the daemon needs (`CORE_LEDS`, the same ones `led::driver_present`
//! checks) and the brightness LED. sysfs (kernfs) gives every node it
//! creates a new inode number -- on 64-bit it carries a generation, so it
//! isn't reused -- so a reload shows up as changed numbers even if the
//! devices are back before the next check. A snapshot has to be seen on
//! two checks in a row before anything is done about it, so a load caught
//! midway (the platform device there, its LEDs not registered yet) settles
//! first: settings are applied 2-4 s after the driver is complete.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the sysfs paths are looked at.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// The `asustor` driver's LEDs that have to exist for it to count as
/// loaded (with the platform device itself) -- as `led::driver_present`.
const CORE_LEDS: [&str; 2] = ["sata1:red:disk", "green:status"];

/// Inode numbers of the watched paths; `None` where a path doesn't exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The platform device, then `CORE_LEDS`.
    core: Vec<Option<u64>>,
    /// `led::FRONT_LED` (only there with it87's `led_pwm=3`).
    brightness: Option<u64>,
}

impl Snapshot {
    /// The `asustor` driver is loaded and its LEDs are registered.
    pub fn present(&self) -> bool {
        self.core.iter().all(Option::is_some)
    }
}

/// What changed, once a new snapshot has settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The driver is there and wasn't before (at startup, or since it went
    /// away).
    Appeared,
    /// Its devices were re-created: the driver was reloaded.
    Reloaded,
    /// Only the front LED dimmer appeared or was re-created (`it87`
    /// loaded or reloaded on its own).
    BrightnessLed,
    /// The driver went away (unloaded, e.g. halfway through a reload that
    /// failed).
    Disappeared,
}

impl Change {
    /// Whether LED settings should be (re)applied now.
    pub fn reapply(self) -> bool {
        !matches!(self, Change::Disappeared)
    }

    /// The `asustor` platform device itself is new, so whatever was read
    /// from it at startup (power settings) is worth reading again.
    pub fn platform_is_new(self) -> bool {
        matches!(self, Change::Appeared | Change::Reloaded)
    }

    /// The log line for it.
    pub fn describe(self) -> &'static str {
        match self {
            Change::Appeared => "platform driver detected; applying LED settings",
            Change::Reloaded => {
                "platform driver reloaded (its devices were re-created); re-applying LED settings"
            }
            Change::BrightnessLed => {
                "front LED dimmer (front_panel::brightness) appeared; re-applying LED settings"
            }
            Change::Disappeared => {
                "platform driver gone (unloaded?); LED settings will be applied again once it's back"
            }
        }
    }
}

/// The sysfs paths looked at; tests point them at a scratch directory.
#[derive(Debug, Clone)]
pub struct Paths {
    platform: PathBuf,
    leds: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Paths {
            platform: crate::platform::PLATFORM_DIR.into(),
            leds: "/sys/class/leds".into(),
        }
    }

    fn snapshot(&self) -> Snapshot {
        // `metadata` follows /sys/class/leds/<name>, a symlink to the LED's
        // directory under its device, so this is that directory's inode.
        let ino = |path: &Path| std::fs::metadata(path).ok().map(|m| m.ino());
        let mut core = vec![ino(&self.platform)];
        core.extend(CORE_LEDS.iter().map(|led| ino(&self.leds.join(led))));
        Snapshot {
            core,
            brightness: ino(&self.leds.join(crate::led::FRONT_LED)),
        }
    }
}

pub struct DriverWatch {
    paths: Paths,
    /// What the LED settings were last applied against (the startup pass,
    /// or the last `Change`).
    applied: Snapshot,
    /// The last snapshot taken, waiting to be seen again before it counts.
    seen: Option<Snapshot>,
    next_check: Instant,
}

impl DriverWatch {
    /// Starts watching from what's there now -- take it right before the
    /// startup LED pass, which counts as applying against this snapshot.
    pub fn new(paths: Paths, now: Instant) -> Self {
        let applied = paths.snapshot();
        DriverWatch {
            paths,
            seen: Some(applied.clone()),
            applied,
            next_check: now + CHECK_INTERVAL,
        }
    }

    /// Takes a snapshot if one is due, and says what changed once it has
    /// settled. Call on every event-loop wakeup; it does nothing between
    /// checks.
    pub fn poll(&mut self, now: Instant) -> Option<Change> {
        if now < self.next_check {
            return None;
        }
        self.next_check = now + CHECK_INTERVAL;
        let snapshot = self.paths.snapshot();
        self.observe(snapshot)
    }

    /// The decision, apart from the clock and the filesystem.
    fn observe(&mut self, snapshot: Snapshot) -> Option<Change> {
        if self.seen.as_ref() != Some(&snapshot) {
            // New: wait for it to be seen once more.
            self.seen = Some(snapshot);
            return None;
        }
        if snapshot == self.applied {
            return None;
        }
        let before = std::mem::replace(&mut self.applied, snapshot);
        let after = &self.applied;
        match (before.present(), after.present()) {
            (false, true) => Some(Change::Appeared),
            (true, false) => Some(Change::Disappeared),
            (true, true) if before.core != after.core => Some(Change::Reloaded),
            // The dimmer comes from it87, which can be (re)loaded with or
            // without the asustor module; it going away needs nothing
            // done (the brightness code reports a missing LED itself).
            _ if after.brightness.is_some() && after.brightness != before.brightness => {
                Some(Change::BrightnessLed)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh scratch directory per test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lcm-status-driver-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            platform: root.join("devices/platform/asustor"),
            leds: root.join("class/leds"),
        }
    }

    fn core_dirs(p: &Paths) -> Vec<PathBuf> {
        let mut dirs = vec![p.platform.clone()];
        dirs.extend(CORE_LEDS.iter().map(|led| p.leds.join(led)));
        dirs
    }

    fn load_asustor(p: &Paths) {
        for dir in core_dirs(p) {
            fs::create_dir_all(dir).unwrap();
        }
    }

    fn unload_asustor(p: &Paths) {
        for dir in core_dirs(p) {
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Re-creates `dir` the way a reload does, as a new node. (Made before
    /// the old one is removed, so the filesystem can't hand the freed
    /// inode number straight back -- some do, unlike kernfs.)
    fn recreate(dir: &Path) {
        let fresh = dir.with_extension("new");
        fs::create_dir_all(&fresh).unwrap();
        fs::remove_dir_all(dir).unwrap();
        fs::rename(&fresh, dir).unwrap();
    }

    /// Polls as the event loop would, `CHECK_INTERVAL` apart, `checks`
    /// times; every change reported.
    fn run(watch: &mut DriverWatch, clock: &mut Instant, checks: usize) -> Vec<Change> {
        (0..checks)
            .filter_map(|_| {
                *clock += CHECK_INTERVAL;
                watch.poll(*clock)
            })
            .collect()
    }

    #[test]
    fn driver_loaded_after_startup_is_noticed_once_it_settles() {
        let root = scratch("late");
        let p = paths(&root);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), clock);
        assert!(!watch.applied.present());
        assert_eq!(run(&mut watch, &mut clock, 3), []);

        load_asustor(&p);
        // First sight only arms it; the second check acts.
        assert_eq!(run(&mut watch, &mut clock, 1), []);
        assert_eq!(run(&mut watch, &mut clock, 1), [Change::Appeared]);
        assert!(watch.applied.present());
        // And only once.
        assert_eq!(run(&mut watch, &mut clock, 5), []);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_happens_between_checks() {
        let root = scratch("interval");
        let p = paths(&root);
        let start = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), start);
        load_asustor(&p);
        for ms in (0..2000).step_by(100) {
            assert_eq!(watch.poll(start + Duration::from_millis(ms)), None);
        }
        assert_eq!(watch.poll(start + CHECK_INTERVAL), None);
        assert_eq!(
            watch.poll(start + CHECK_INTERVAL * 2),
            Some(Change::Appeared)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_half_loaded_driver_is_not_acted_on() {
        let root = scratch("partial");
        let p = paths(&root);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), clock);
        // The platform device registered, its LEDs not (yet).
        fs::create_dir_all(&p.platform).unwrap();
        assert_eq!(run(&mut watch, &mut clock, 3), []);
        fs::create_dir_all(p.leds.join("sata1:red:disk")).unwrap();
        fs::create_dir_all(p.leds.join("green:status")).unwrap();
        assert_eq!(run(&mut watch, &mut clock, 2), [Change::Appeared]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn present_at_startup_means_nothing_to_do() {
        let root = scratch("present");
        let p = paths(&root);
        load_asustor(&p);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p, clock);
        assert!(watch.applied.present());
        assert_eq!(run(&mut watch, &mut clock, 5), []);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_reload_is_noticed_even_when_quicker_than_a_check() {
        let root = scratch("reload");
        let p = paths(&root);
        load_asustor(&p);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), clock);
        for dir in core_dirs(&p) {
            recreate(&dir);
        }
        assert_eq!(run(&mut watch, &mut clock, 3), [Change::Reloaded]);
        assert!(watch.applied.present());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unload_then_load_is_reported_both_ways() {
        let root = scratch("unload");
        let p = paths(&root);
        load_asustor(&p);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), clock);
        unload_asustor(&p);
        assert_eq!(run(&mut watch, &mut clock, 3), [Change::Disappeared]);
        assert!(!watch.applied.present());
        load_asustor(&p);
        assert_eq!(run(&mut watch, &mut clock, 3), [Change::Appeared]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_brightness_led_on_its_own_counts_too() {
        let root = scratch("dimmer");
        let p = paths(&root);
        load_asustor(&p);
        let mut clock = Instant::now();
        let mut watch = DriverWatch::new(p.clone(), clock);
        let dimmer = p.leds.join(crate::led::FRONT_LED);
        fs::create_dir_all(&dimmer).unwrap();
        assert_eq!(run(&mut watch, &mut clock, 3), [Change::BrightnessLed]);
        recreate(&dimmer);
        assert_eq!(run(&mut watch, &mut clock, 3), [Change::BrightnessLed]);
        // Going away needs nothing applied.
        fs::remove_dir_all(&dimmer).unwrap();
        assert_eq!(run(&mut watch, &mut clock, 3), []);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn what_each_change_asks_for() {
        assert!(Change::Appeared.reapply() && Change::Appeared.platform_is_new());
        assert!(Change::Reloaded.reapply() && Change::Reloaded.platform_is_new());
        assert!(Change::BrightnessLed.reapply() && !Change::BrightnessLed.platform_is_new());
        assert!(!Change::Disappeared.reapply());
        assert_eq!(
            Change::Appeared.describe(),
            "platform driver detected; applying LED settings"
        );
    }
}
