//! CPU package power limits (Intel RAPL): applies `[cpu_power]`'s PL1 (the
//! sustained limit), PL2 (the burst limit) and tau (PL1's averaging window)
//! through the `intel-rapl:0` powercap zone, and puts the stock values
//! back while any temperature sensor is critical.
//!
//! The limits live in the CPU and reset on reboot, so they are applied at
//! startup and rechecked every `VERIFY_INTERVAL` (firmware or another tool
//! may rewrite them). Like the fans this fails toward cooling:
//!
//! - **A critical sensor drops the limits to stock.** Every connected
//!   hwmon sensor counts, each against its own chip's critical threshold
//!   (`config::resolve_temp_threshold`, the same resolution the health
//!   monitor uses). The configured limits come back only after every
//!   sensor has been under its *warning* threshold for `rearm_secs`, so a
//!   sensor hovering at critical can't flap the limits.
//! - **No fresh readings count as critical.** A sensor sweep that stops
//!   (a wedged drive read) or finds nothing leaves the limits at stock.
//! - **A failed write is retried every second**, and logged once.
//! - **The daemon stopping, however it stops, restores stock**
//!   (`restore_stock`, run by `lcm-status fan-failsafe` from the unit's
//!   `ExecStopPost`), and the main loop treats this thread dying or
//!   stalling like the fan thread's.
//!
//! Sensors are swept on their own thread (like the fan readers), so a hung
//! `drivetemp` read only makes the sample stale; the control thread never
//! waits on one.

use crate::config::{CpuPowerConfig, TemperatureConfig, resolve_temp_threshold};
use crate::fan::{FAN_HEARTBEAT_TIMEOUT, FanLiveness, lock};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ZONE_DIR: &str = "/sys/class/powercap/intel-rapl:0";
const PL1_FILE: &str = "constraint_0_power_limit_uw";
const PL2_FILE: &str = "constraint_1_power_limit_uw";
const TAU_FILE: &str = "constraint_0_time_window_us";
const UW_PER_W: u64 = 1_000_000;
const US_PER_S: u64 = 1_000_000;

/// How often the control thread re-reads the limits to see whether
/// something else changed them.
const VERIFY_INTERVAL: Duration = Duration::from_secs(60);
/// The control thread's loop period, and so its heartbeat period.
const TICK: Duration = Duration::from_secs(1);
/// How long the first sensor sweep is waited for at startup.
const FIRST_SWEEP_WAIT: Duration = Duration::from_secs(10);
/// A sample at least this old (or three sweep periods, if longer) is no
/// reading at all.
const MIN_STALE_AFTER: Duration = Duration::from_secs(30);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

/// One temperature sensor: (chip name, description, degrees C), as
/// `hal::all_connected_temps` returns them.
pub type Reading = (String, String, f32);

/// A PL1/PL2/tau triple in the kernel's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub pl1_uw: u64,
    pub pl2_uw: u64,
    pub tau_us: u64,
}

impl Limits {
    fn configured(cfg: &CpuPowerConfig) -> Self {
        Limits {
            pl1_uw: u64::from(cfg.pl1_w) * UW_PER_W,
            pl2_uw: u64::from(cfg.pl2_w) * UW_PER_W,
            tau_us: u64::from(cfg.tau_secs) * US_PER_S,
        }
    }

    fn stock(cfg: &CpuPowerConfig) -> Self {
        Limits {
            pl1_uw: u64::from(cfg.stock_pl1_w) * UW_PER_W,
            pl2_uw: u64::from(cfg.stock_pl2_w) * UW_PER_W,
            tau_us: u64::from(cfg.stock_tau_secs) * US_PER_S,
        }
    }

    /// Whether `read` (what the zone reports) is this setting. The kernel
    /// rounds the time window to the hardware's steps (stock reads back as
    /// 27983872us, not 28000000), so that one gets 5% either way.
    fn matches(&self, read: &Limits) -> bool {
        self.pl1_uw == read.pl1_uw
            && self.pl2_uw == read.pl2_uw
            && self.tau_us.abs_diff(read.tau_us) * 20 <= self.tau_us
    }

    fn describe(&self) -> String {
        format!(
            "PL1 {}W, PL2 {}W, tau {}s",
            self.pl1_uw / UW_PER_W,
            self.pl2_uw / UW_PER_W,
            self.tau_us / US_PER_S
        )
    }
}

/// The order to write `new` in, given the zone's current PL2. PL2 is
/// raised before PL1 and lowered after it, so PL1 never exceeds PL2 in
/// between (some kernels reject that).
fn write_plan(cur_pl2_uw: u64, new: &Limits) -> [(&'static str, u64); 3] {
    let (pl1, pl2) = ((PL1_FILE, new.pl1_uw), (PL2_FILE, new.pl2_uw));
    let tau = (TAU_FILE, new.tau_us);
    if new.pl2_uw >= cur_pl2_uw {
        [pl2, pl1, tau]
    } else {
        [pl1, pl2, tau]
    }
}

/// One RAPL powercap zone directory.
pub struct Zone {
    dir: PathBuf,
}

impl Zone {
    pub fn system() -> Self {
        Zone::new(ZONE_DIR)
    }

    fn new(dir: impl Into<PathBuf>) -> Self {
        Zone { dir: dir.into() }
    }

    fn read_raw(&self, file: &str) -> io::Result<String> {
        std::fs::read_to_string(self.dir.join(file))
            .map(|s| s.trim().to_string())
            .map_err(|e| io::Error::new(e.kind(), format!("{file}: {e}")))
    }

    fn read_u64(&self, file: &str) -> io::Result<u64> {
        self.read_raw(file)?
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{file}: {e}")))
    }

    /// Refuses a zone whose constraints aren't the package's long and
    /// short term limits, so a different layout is never written blindly.
    fn check(&self) -> Result<(), String> {
        for (file, want) in [
            ("constraint_0_name", "long_term"),
            ("constraint_1_name", "short_term"),
        ] {
            match self.read_raw(file) {
                Ok(name) if name == want => {}
                Ok(name) => {
                    return Err(format!(
                        "{}/{file} is \"{name}\", expected \"{want}\"",
                        self.dir.display()
                    ));
                }
                Err(e) => return Err(format!("{}: {e}", self.dir.display())),
            }
        }
        Ok(())
    }

    fn read(&self) -> io::Result<Limits> {
        Ok(Limits {
            pl1_uw: self.read_u64(PL1_FILE)?,
            pl2_uw: self.read_u64(PL2_FILE)?,
            tau_us: self.read_u64(TAU_FILE)?,
        })
    }

    fn write(&self, new: &Limits) -> io::Result<()> {
        let cur_pl2 = self.read_u64(PL2_FILE)?;
        for (file, value) in write_plan(cur_pl2, new) {
            std::fs::write(self.dir.join(file), value.to_string())
                .map_err(|e| io::Error::new(e.kind(), format!("writing {file}: {e}")))?;
        }
        Ok(())
    }
}

/// Puts the stock limits back, if `[cpu_power]` is enabled. Run by
/// `lcm-status fan-failsafe` after every daemon stop, so a killed or
/// crashed daemon never leaves raised limits with nothing watching
/// temperatures.
pub fn restore_stock(cfg: &CpuPowerConfig) -> Result<(), String> {
    if !cfg.enabled {
        return Ok(());
    }
    let zone = Zone::system();
    zone.check()?;
    let stock = Limits::stock(cfg);
    zone.write(&stock).map_err(|e| e.to_string())?;
    crate::syslog::info(&format!(
        "CPU power limits restored to stock ({})",
        stock.describe()
    ));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Configured,
    Stock,
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// A sensor went critical (why), or there are no readings.
    Tripped(String),
    Rearmed,
}

/// Decides between the configured and the stock limits from the sensors.
struct Guard {
    rearm: Duration,
    tripped: bool,
    /// Since when every sensor has been under its warning threshold.
    cool_since: Option<Instant>,
}

impl Guard {
    fn new(rearm: Duration) -> Self {
        Guard {
            rearm,
            tripped: false,
            cool_since: None,
        }
    }

    fn mode(&self) -> Mode {
        if self.tripped {
            Mode::Stock
        } else {
            Mode::Configured
        }
    }

    /// `readings` is `None` for no fresh sample. Returns what changed.
    fn observe(
        &mut self,
        now: Instant,
        readings: Option<&[Reading]>,
        temps: &TemperatureConfig,
    ) -> Option<Change> {
        let readings = readings.filter(|r| !r.is_empty());
        let critical = match readings {
            None => Some("no fresh temperature readings".to_string()),
            Some(r) => r
                .iter()
                .find(|(chip, _, t)| *t >= resolve_temp_threshold(temps, chip).1)
                .map(|(chip, label, t)| {
                    let crit = resolve_temp_threshold(temps, chip).1;
                    format!("{label}: {t:.1}C at or above critical ({crit:.1}C)")
                }),
        };
        if let Some(why) = critical {
            self.cool_since = None;
            let was = std::mem::replace(&mut self.tripped, true);
            return (!was).then_some(Change::Tripped(why));
        }
        if !self.tripped {
            return None;
        }
        // Not NaN-safe on purpose: a NaN is never "below warning", so it
        // can't re-arm.
        let cool = readings.is_some_and(|r| {
            r.iter()
                .all(|(chip, _, t)| *t < resolve_temp_threshold(temps, chip).0)
        });
        if !cool {
            self.cool_since = None;
            return None;
        }
        let since = *self.cool_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= self.rearm {
            self.tripped = false;
            self.cool_since = None;
            return Some(Change::Rearmed);
        }
        None
    }
}

/// The guard plus what has actually been written to the zone.
struct Controller {
    zone: Zone,
    configured: Limits,
    stock: Limits,
    guard: Guard,
    /// What was last written successfully; `None` forces a write.
    applied: Option<Mode>,
    last_verify: Option<Instant>,
    /// Why the current trip happened, for the status line.
    trip_reason: Option<String>,
    /// The write problem last logged, so it is logged once.
    problem: Option<String>,
}

impl Controller {
    fn new(zone: Zone, cfg: &CpuPowerConfig) -> Self {
        Controller {
            zone,
            configured: Limits::configured(cfg),
            stock: Limits::stock(cfg),
            guard: Guard::new(Duration::from_secs(cfg.rearm_secs)),
            applied: None,
            last_verify: None,
            trip_reason: None,
            problem: None,
        }
    }

    fn limits(&self, mode: Mode) -> Limits {
        match mode {
            Mode::Configured => self.configured,
            Mode::Stock => self.stock,
        }
    }

    fn tick(&mut self, now: Instant, readings: Option<&[Reading]>, temps: &TemperatureConfig) {
        match self.guard.observe(now, readings, temps) {
            Some(Change::Tripped(why)) => {
                crate::syslog::critical(&format!(
                    "{why}: CPU power limits dropping to stock ({})",
                    self.stock.describe()
                ));
                self.trip_reason = Some(why);
            }
            Some(Change::Rearmed) => {
                crate::syslog::notice(&format!(
                    "temperatures back under warning: CPU power limits returning to {}",
                    self.configured.describe()
                ));
                self.trip_reason = None;
            }
            None => {}
        }
        let want = self.guard.mode();
        self.verify(now);
        if self.applied != Some(want) {
            self.apply(now, want);
        }
    }

    /// Notices limits changed behind our back and forces a rewrite.
    fn verify(&mut self, now: Instant) {
        let Some(applied) = self.applied else {
            return;
        };
        if self
            .last_verify
            .is_some_and(|t| now.saturating_duration_since(t) < VERIFY_INTERVAL)
        {
            return;
        }
        self.last_verify = Some(now);
        let want = self.limits(applied);
        match self.zone.read() {
            Ok(cur) if want.matches(&cur) => {}
            Ok(cur) => {
                crate::syslog::warning(&format!(
                    "CPU power limits changed to {} (expected {}); reapplying",
                    cur.describe(),
                    want.describe()
                ));
                self.applied = None;
            }
            Err(e) => {
                crate::syslog::warning(&format!("cannot read CPU power limits: {e}; reapplying"));
                self.applied = None;
            }
        }
    }

    fn apply(&mut self, now: Instant, mode: Mode) {
        let limits = self.limits(mode);
        let result = self
            .zone
            .check()
            .and_then(|()| self.zone.write(&limits).map_err(|e| e.to_string()));
        match result {
            Ok(()) => {
                self.applied = Some(mode);
                self.last_verify = Some(now);
                let which = match mode {
                    Mode::Configured => "configured",
                    Mode::Stock => "stock",
                };
                crate::syslog::info(&format!(
                    "CPU power limits set to {which}: {}",
                    limits.describe()
                ));
                if self.problem.take().is_some() {
                    crate::syslog::notice("CPU power limits: writes work again");
                }
            }
            Err(e) => {
                if self.problem.as_deref() != Some(e.as_str()) {
                    let msg = format!("cannot set CPU power limits to {}: {e}", limits.describe());
                    match mode {
                        Mode::Stock => crate::syslog::critical(&msg),
                        Mode::Configured => crate::syslog::warning(&msg),
                    }
                    self.problem = Some(e);
                }
            }
        }
    }

    fn status_line(&self) -> String {
        if let Some(problem) = &self.problem {
            return format!("NOT APPLIED: {problem}");
        }
        match (self.applied, &self.trip_reason) {
            (Some(Mode::Configured), _) => format!("{} (configured)", self.configured.describe()),
            (Some(Mode::Stock), reason) => format!(
                "{} (STOCK: {})",
                self.stock.describe(),
                reason.as_deref().unwrap_or("cooling down")
            ),
            (None, _) => "not applied yet".to_string(),
        }
    }
}

type Sample = Option<(Instant, Vec<Reading>)>;

/// Sleeps `total`, in short slices, returning early once `stop` is set.
fn sleep_unless(stop: &AtomicBool, total: Duration) {
    let end = Instant::now().checked_add(total);
    while !stop.load(Ordering::Relaxed) && end.is_none_or(|e| Instant::now() < e) {
        thread::sleep(Duration::from_millis(200));
    }
}

/// The CPU power limits' two threads. Owned by `run_daemon`; the main loop
/// only sees the status line and the control thread's liveness.
pub struct CpuPowerService {
    status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    control: JoinHandle<()>,
    liveness: FanLiveness,
}

impl CpuPowerService {
    pub fn spawn(cfg: &CpuPowerConfig, temps: &TemperatureConfig) -> io::Result<Self> {
        let status = Arc::new(Mutex::new("starting".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let liveness = FanLiveness::new();
        let latest: Arc<Mutex<Sample>> = Arc::new(Mutex::new(None));
        let sweep_every = Duration::from_secs(cfg.check_secs);

        thread::Builder::new()
            .name("cpu-power-sensors".into())
            .spawn({
                let (latest, stop) = (Arc::clone(&latest), Arc::clone(&stop));
                move || {
                    while !stop.load(Ordering::Relaxed) {
                        let readings = crate::hal::all_connected_temps();
                        *lock(&latest) = Some((Instant::now(), readings));
                        sleep_unless(&stop, sweep_every);
                    }
                }
            })?;

        let stale_after = MIN_STALE_AFTER.max(sweep_every.saturating_mul(3));
        let control = thread::Builder::new()
            .name("cpu-power".into())
            .spawn({
                let (status, stop, liveness) =
                    (Arc::clone(&status), Arc::clone(&stop), liveness.clone());
                let (cfg, temps) = (cfg.clone(), temps.clone());
                move || {
                    let mut ctl = Controller::new(Zone::system(), &cfg);
                    let first = Instant::now();
                    while lock(&latest).is_none()
                        && first.elapsed() < FIRST_SWEEP_WAIT
                        && !stop.load(Ordering::Relaxed)
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    while !stop.load(Ordering::Relaxed) {
                        let readings = lock(&latest)
                            .as_ref()
                            .filter(|(at, _)| at.elapsed() <= stale_after)
                            .map(|(_, r)| r.clone());
                        ctl.tick(Instant::now(), readings.as_deref(), &temps);
                        *lock(&status) = ctl.status_line();
                        liveness.beat();
                        thread::sleep(TICK);
                    }
                }
            })
            .inspect_err(|_| stop.store(true, Ordering::Relaxed))?;
        Ok(CpuPowerService {
            status,
            stop,
            control,
            liveness,
        })
    }

    pub fn status_line(&self) -> String {
        lock(&self.status).clone()
    }

    /// True if the control thread exited on its own (it panicked).
    pub fn has_died(&self) -> bool {
        self.control.is_finished()
    }

    /// True if the control thread is alive but has stopped making progress.
    pub fn is_stalled(&self) -> bool {
        self.liveness.age() > FAN_HEARTBEAT_TIMEOUT
    }

    /// Stops both threads. The limits stay as they are: `restore_stock`
    /// (from the unit's `ExecStopPost`) is what puts stock back.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        while !self.control.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temps() -> TemperatureConfig {
        TemperatureConfig::default() // coretemp 85/100, drivetemp 50/60, ...
    }

    fn r(chip: &str, t: f32) -> Reading {
        (chip.to_string(), format!("{chip} temp1"), t)
    }

    fn cfg() -> CpuPowerConfig {
        CpuPowerConfig {
            enabled: true,
            ..CpuPowerConfig::default()
        }
    }

    /// A zone directory holding stock values, removed on drop.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("lcm-status-rapl-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create fixture dir");
            for (file, content) in [
                ("constraint_0_name", "long_term"),
                ("constraint_1_name", "short_term"),
                (PL1_FILE, "10000000"),
                (PL2_FILE, "25000000"),
                (TAU_FILE, "27983872"),
            ] {
                std::fs::write(dir.join(file), format!("{content}\n")).expect("write attr");
            }
            Fixture(dir)
        }

        fn zone(&self) -> Zone {
            Zone::new(&self.0)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pl2_is_raised_before_pl1_and_lowered_after_it() {
        let lim = |pl1, pl2| Limits {
            pl1_uw: pl1,
            pl2_uw: pl2,
            tau_us: 1,
        };
        let order = |cur_pl2, new: &Limits| write_plan(cur_pl2, new).map(|(f, _)| f);
        assert_eq!(
            order(14, &lim(10, 25)),
            [PL2_FILE, PL1_FILE, TAU_FILE],
            "stock restore raises PL2"
        );
        assert_eq!(
            order(25, &lim(12, 14)),
            [PL1_FILE, PL2_FILE, TAU_FILE],
            "configured lowers PL2"
        );
    }

    #[test]
    fn tau_readback_tolerates_kernel_rounding() {
        let stock = Limits::stock(&CpuPowerConfig::default());
        let mut read = stock;
        read.tau_us = 27_983_872;
        assert!(stock.matches(&read));
        read.tau_us = 40_000_000;
        assert!(!stock.matches(&read));
        read.tau_us = stock.tau_us;
        read.pl2_uw += 1;
        assert!(!stock.matches(&read), "power limits must match exactly");
    }

    #[test]
    fn writes_the_configured_limits_and_reads_them_back() {
        let fx = Fixture::new("write");
        let zone = fx.zone();
        zone.check().expect("fixture layout");
        let want = Limits::configured(&cfg());
        zone.write(&want).expect("write");
        assert_eq!(zone.read().expect("read"), want);
    }

    #[test]
    fn a_zone_with_another_layout_is_refused() {
        let fx = Fixture::new("layout");
        std::fs::write(fx.0.join("constraint_1_name"), "peak_power\n").expect("rename");
        assert!(fx.zone().check().is_err());
        let missing = Zone::new(fx.0.join("absent"));
        assert!(missing.check().is_err());
    }

    #[test]
    fn a_critical_sensor_trips_and_a_warning_one_does_not() {
        let mut g = Guard::new(Duration::from_secs(60));
        let now = Instant::now();
        let warn_only = [r("coretemp", 90.0), r("drivetemp", 55.0)];
        assert_eq!(g.observe(now, Some(&warn_only), &temps()), None);
        assert_eq!(g.mode(), Mode::Configured);
        let hot = [r("coretemp", 60.0), r("drivetemp", 60.0)];
        assert!(matches!(
            g.observe(now, Some(&hot), &temps()),
            Some(Change::Tripped(_))
        ));
        assert_eq!(g.mode(), Mode::Stock);
        // Still critical: no second trip event.
        assert_eq!(g.observe(now, Some(&hot), &temps()), None);
    }

    #[test]
    fn each_chip_uses_its_own_critical_threshold() {
        // 65C is nothing for a CPU but critical for an NVMe (70) only at 70.
        let mut g = Guard::new(Duration::ZERO);
        let now = Instant::now();
        assert_eq!(g.observe(now, Some(&[r("coretemp", 99.9)]), &temps()), None);
        assert!(
            g.observe(now, Some(&[r("coretemp", 100.0)]), &temps())
                .is_some()
        );
    }

    #[test]
    fn no_readings_is_treated_as_critical() {
        let now = Instant::now();
        for readings in [None, Some(&[][..])] {
            let mut g = Guard::new(Duration::from_secs(60));
            assert!(matches!(
                g.observe(now, readings, &temps()),
                Some(Change::Tripped(_))
            ));
            assert_eq!(g.mode(), Mode::Stock);
        }
    }

    #[test]
    fn rearms_only_after_every_sensor_stays_under_warning_for_the_hold() {
        let mut g = Guard::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let hot = [r("coretemp", 101.0)];
        g.observe(t0, Some(&hot), &temps());
        // Under critical but over warning (85): not cool.
        let warm = [r("coretemp", 90.0)];
        assert_eq!(g.observe(at(10), Some(&warm), &temps()), None);
        // Cool starts here.
        let cool = [r("coretemp", 70.0), r("drivetemp", 40.0)];
        assert_eq!(g.observe(at(20), Some(&cool), &temps()), None);
        assert_eq!(g.observe(at(79), Some(&cool), &temps()), None);
        assert_eq!(g.mode(), Mode::Stock);
        // A warm blip restarts the hold.
        assert_eq!(g.observe(at(80), Some(&warm), &temps()), None);
        assert_eq!(g.observe(at(81), Some(&cool), &temps()), None);
        assert_eq!(g.observe(at(140), Some(&cool), &temps()), None);
        assert_eq!(
            g.observe(at(141), Some(&cool), &temps()),
            Some(Change::Rearmed)
        );
        assert_eq!(g.mode(), Mode::Configured);
    }

    #[test]
    fn a_nan_reading_never_rearms() {
        let mut g = Guard::new(Duration::ZERO);
        let now = Instant::now();
        g.observe(now, Some(&[r("coretemp", 101.0)]), &temps());
        assert_eq!(
            g.observe(now, Some(&[r("coretemp", f32::NAN)]), &temps()),
            None
        );
        assert_eq!(g.mode(), Mode::Stock);
    }

    #[test]
    fn controller_applies_configured_drops_to_stock_and_comes_back() {
        let fx = Fixture::new("ctl");
        let mut c = Controller::new(fx.zone(), &cfg());
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let cool = [r("coretemp", 50.0)];

        c.tick(t0, Some(&cool), &temps());
        assert_eq!(fx.zone().read().expect("read"), c.configured);

        c.tick(at(1), Some(&[r("coretemp", 100.0)]), &temps());
        assert_eq!(fx.zone().read().expect("read").pl2_uw, 25_000_000);
        assert!(c.status_line().contains("STOCK"), "{}", c.status_line());

        c.tick(at(2), Some(&cool), &temps());
        assert_eq!(fx.zone().read().expect("read").pl2_uw, 25_000_000);
        c.tick(at(63), Some(&cool), &temps());
        assert_eq!(fx.zone().read().expect("read"), c.configured);
        assert!(c.status_line().contains("(configured)"));
    }

    #[test]
    fn a_critical_reading_at_the_very_first_tick_never_applies_the_configured_limits() {
        let fx = Fixture::new("hotstart");
        let mut c = Controller::new(fx.zone(), &cfg());
        c.tick(Instant::now(), Some(&[r("drivetemp", 61.0)]), &temps());
        let read = fx.zone().read().expect("read");
        assert_eq!((read.pl1_uw, read.pl2_uw), (10_000_000, 25_000_000));
    }

    #[test]
    fn a_failed_write_is_retried_until_it_works() {
        let fx = Fixture::new("retry");
        let mut c = Controller::new(Zone::new(fx.0.join("absent")), &cfg());
        let now = Instant::now();
        c.tick(now, Some(&[r("coretemp", 50.0)]), &temps());
        assert!(
            c.status_line().starts_with("NOT APPLIED"),
            "{}",
            c.status_line()
        );
        c.zone = fx.zone();
        c.tick(now, Some(&[r("coretemp", 50.0)]), &temps());
        assert!(c.status_line().contains("(configured)"));
        assert_eq!(fx.zone().read().expect("read"), c.configured);
    }

    #[test]
    fn limits_changed_behind_our_back_are_rewritten() {
        let fx = Fixture::new("drift");
        let mut c = Controller::new(fx.zone(), &cfg());
        let t0 = Instant::now();
        let cool = [r("coretemp", 50.0)];
        c.tick(t0, Some(&cool), &temps());
        std::fs::write(fx.0.join(PL1_FILE), "10000000").expect("meddle");
        // Within the verify interval nothing is rechecked...
        c.tick(t0 + Duration::from_secs(1), Some(&cool), &temps());
        c.tick(t0 + Duration::from_secs(2), Some(&cool), &temps());
        assert_eq!(fx.zone().read().expect("read").pl1_uw, 10_000_000);
        // ...after it, the drift is found and fixed.
        c.tick(
            t0 + VERIFY_INTERVAL + Duration::from_secs(3),
            Some(&cool),
            &temps(),
        );
        assert_eq!(fx.zone().read().expect("read"), c.configured);
    }
}
