//! The daemon's state machine: status rotation, the action menu, confirm
//! flow, socket overrides, locate, and sleep -- all in one place so the
//! interactions between them (e.g. "a critical alert can preempt rotation
//! but not a confirm screen") are enforced in one spot, not scattered
//! across the event loop.

use crate::config::{Category, Config};
use crate::hal::{self, Screen};
use crate::led;
use crate::protocol::Key;
use crate::socket::{Level, SocketCommand};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Shutdown,
    Restart,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Shutdown => "SHUTDOWN",
            Action::Restart => "RESTART",
        }
    }
}

#[derive(Debug, Clone)]
struct Override {
    level: Level,
    expires_at: Option<Instant>,
    bay: Option<u32>,
    line0: String,
    line1: String,
}

/// A chassis-wide `LOCATE` (one with no bay): power and status LEDs
/// flashing, and the hostname on the LCD, until `until` (`None`: until
/// `LOCATE off`).
#[derive(Debug)]
struct ChassisLocate {
    until: Option<Instant>,
    /// Read once when the locate starts, not on every render.
    hostname: String,
}

impl ChassisLocate {
    /// The LCD text: the hostname -- the thing that tells two identical
    /// boxes in a rack apart -- over "LOCATE" and the time left, if any.
    fn screen(&self) -> (String, String) {
        let line1 = match self.until {
            Some(t) => format!("LOCATE {}s", secs_left(t)),
            None => "LOCATE".to_string(),
        };
        let line0 = if self.hostname.is_empty() {
            "LOCATE".to_string()
        } else {
            self.hostname.clone()
        };
        (line0, line1)
    }
}

/// Whole seconds until `t`, rounded up so a countdown never shows 0s
/// while it's still running.
fn secs_left(t: Instant) -> u64 {
    let left = t.saturating_duration_since(Instant::now());
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

#[derive(Debug)]
enum Mode {
    /// Normal operation: paging/rotating through gathered screens.
    Status,
    /// ENTER was pressed from Status; choosing an action.
    ActionMenu {
        options: Vec<Action>,
        selection: usize,
    },
    /// An action was selected; awaiting confirm/cancel.
    Confirm { action: Action, deadline: Instant },
    /// A `[[power_schedule]]` shutdown/restart is about to happen: counts
    /// down to `deadline`, then runs `action`; any key cancels it.
    Countdown {
        action: Action,
        deadline: Instant,
        /// Which rule, for syslog.
        label: String,
    },
}

/// A scroll cursor for a line whose text overflows 16 chars.
#[derive(Debug, Default)]
struct Scroll {
    text: String,
    offset: usize,
    last_step: Option<Instant>,
    started: Option<Instant>,
    completed_a_pass: bool,
}

/// Per-category cache + its own last-refresh clock, so each category can
/// respect its own [refresh] floor (e.g. HDD/SMART refreshed far less
/// often than network) instead of one blanket interval for everything.
#[derive(Default)]
struct CategoryCache {
    screens: Vec<Screen>,
    last_refresh: Option<Instant>,
}

impl CategoryCache {
    fn stale(&self, floor: Duration) -> bool {
        self.last_refresh.is_none_or(|t| t.elapsed() >= floor)
    }
}

pub struct AppState {
    cfg: Config,
    network_cache: CategoryCache,
    pools_cache: CategoryCache,
    hdd_cache: CategoryCache,
    temperature_cache: CategoryCache,
    docker_cache: CategoryCache,
    screens: Vec<Screen>,
    index: usize,
    auto_rotate: bool,
    resume_at: Option<Instant>,
    last_dwell: Instant,
    mode: Mode,
    over: Option<Override>,
    pending_over: Option<Override>,
    /// The bay currently flashing due to an active override, if any --
    /// tracked separately from `over.bay` so we know which bay's LED to
    /// restore to Normal when the override changes or clears.
    alert_bay: Option<u32>,
    /// Bays an active `LOCATE bay=N` is blinking, each with when it ends
    /// (`None`: until `LOCATE off`). Layered over the health/alert state by
    /// `bay_led_state` rather than replacing it, so a bay goes back to
    /// whatever that state has become by the time its locate ends -- e.g.
    /// a SMART failure found mid-locate shows the moment it's over.
    locate_bays: BTreeMap<u32, Option<Instant>>,
    locate_chassis: Option<ChassisLocate>,
    sleeping: bool,
    schedule_wants_sleep: bool,
    awake_override_until: Option<Instant>,
    scroll0: Scroll,
    scroll1: Scroll,
    monitor: crate::monitor::HealthMonitor,
    /// Worst current fan health across every configured fan -- pushed in
    /// each tick from main.rs, which reads it from `fan::FanService` (fan
    /// control runs on its own thread, not inside `AppState`). Current
    /// state, not transition-gated: this feeds `recompute_status_led`,
    /// while syslog transitions are logged by `FanController` itself.
    fan_health: Level,
    /// Last `zpool list`, refreshed on the pools cadence. `None` if
    /// `zpool` failed.
    pools: Option<Vec<hal::Pool>>,
    /// Last disk/SMART scan, refreshed whenever pools or HDD are due.
    disks: Vec<hal::Disk>,
    /// The NIC LED problem last logged, if it hasn't cleared since -- see
    /// `note_nic_leds`.
    nic_led_problem: Option<String>,
    /// Daytime front LED brightness as a raw `pwm3` duty: `[led]
    /// brightness`, or with only `night_brightness` set, whatever the BIOS
    /// left at startup (so waking puts back exactly that). `None` when
    /// neither is configured -- `pwm3` is then never touched.
    day_brightness: Option<u8>,
    /// Last duty `apply_front_brightness` wrote, to skip repeat writes.
    applied_brightness: Cell<Option<u8>>,
}

/// Everything that feeds the status LED, plus the resulting verdict --
/// see `AppState::health_summary`.
pub struct HealthSummary {
    pub pool_healths: Vec<(String, String)>,
    pub pool_degraded: bool,
    pub pool_faulted: bool,
    pub bay_failed: bool,
    pub temp_level: Level,
    pub network_level: Level,
    pub fan_health: Level,
    pub overall: Level,
    pub pattern: led::StatusPattern,
}

/// What the event loop should actually do this tick.
pub enum Effect {
    Render(String, String),
    RunAction(Action),
    None,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        AppState {
            cfg,
            network_cache: CategoryCache::default(),
            pools_cache: CategoryCache::default(),
            hdd_cache: CategoryCache::default(),
            temperature_cache: CategoryCache::default(),
            docker_cache: CategoryCache::default(),
            screens: Vec::new(),
            index: 0,
            auto_rotate: true,
            resume_at: None,
            last_dwell: Instant::now(),
            mode: Mode::Status,
            over: None,
            pending_over: None,
            alert_bay: None,
            locate_bays: BTreeMap::new(),
            locate_chassis: None,
            day_brightness: None,
            applied_brightness: Cell::new(None),
            sleeping: false,
            schedule_wants_sleep: false,
            awake_override_until: None,
            scroll0: Scroll::default(),
            scroll1: Scroll::default(),
            monitor: crate::monitor::HealthMonitor::new(),
            fan_health: Level::Info,
            pools: None,
            disks: Vec::new(),
            nic_led_problem: None,
        }
    }

    /// Called every tick from main.rs with the worst current level across
    /// all configured `FanController`s -- see the `fan_health` field doc.
    pub fn set_fan_health(&mut self, level: Level) {
        self.fan_health = level;
    }

    /// Applies the front LEDs' initial state at daemon startup: NIC mode
    /// from config, and a first pass of the health-driven status/bay LEDs
    /// so they're not left in whatever the driver's own boot defaults were.
    pub fn init_leds(&mut self) {
        // The full daytime state, not just the NIC mode: a previous run
        // stopped mid-night-mode leaves the power LED, LAN rail and USB
        // LED dark, and nothing else here turns them back on -- only
        // waking does, and a fresh start was never asleep. (Found live: a
        // restart at night with `night_brightness` set left power and LAN
        // dark until morning.) If the schedule wants night mode, the first
        // tick enters it from here as usual.
        let result = led::exit_night_mode(self.cfg.led.nic_mode, true);
        self.note_nic_leds(result);
        if let Some(mode) = self.cfg.led.bay_mode
            && let Err(e) = led::set_bay_mode(mode)
        {
            crate::syslog::warning(&format!("[led] bay_mode not applied: {e}"));
        }
        self.day_brightness = match (self.cfg.led.brightness, self.cfg.led.night_brightness) {
            (Some(pct), _) => Some(led::brightness_pwm(pct)),
            (None, Some(_)) => led::read_front_brightness(),
            (None, None) => None,
        };
        self.apply_front_brightness();
        self.update_health_leds();
    }

    /// Whether night mode switches the front (status/power/LAN/USB) LEDs
    /// off, rather than dimming them to `[led] night_brightness`.
    fn night_darkens_front(&self) -> bool {
        self.cfg.led.night_brightness.is_none()
    }

    /// Writes the front LED brightness for the current state: the night
    /// level while asleep (if one is configured), except during a chassis
    /// locate, which is meant to be seen; otherwise the day level.
    fn apply_front_brightness(&self) {
        let night = self.cfg.led.night_brightness.map(led::brightness_pwm);
        let want = match night {
            Some(n) if self.sleeping && self.locate_chassis.is_none() => Some(n),
            _ => self.day_brightness,
        };
        let Some(pwm) = want else { return };
        if self.applied_brightness.get() == Some(pwm) {
            return;
        }
        match led::set_front_brightness(pwm) {
            Ok(()) => self.applied_brightness.set(Some(pwm)),
            Err(e) => crate::syslog::warning(&format!("front LED brightness not applied: {e}")),
        }
    }

    /// Whether the LCD's display should be on: always, except asleep with
    /// `[sleep] lcd_off` -- and even then a chassis locate lights it.
    pub fn display_wanted(&self) -> bool {
        !(self.sleeping && self.cfg.sleep.lcd_off && self.locate_chassis.is_none())
    }

    /// Logs a NIC LED failure (see `led::set_nic_mode`) once per distinct
    /// problem rather than on every application -- startup, every sleep
    /// and every wake would otherwise repeat the same line daily -- and
    /// once more when it clears.
    fn note_nic_leds(&mut self, result: Result<(), String>) {
        match result {
            Err(problem) => {
                if self.nic_led_problem.as_ref() != Some(&problem) {
                    crate::syslog::warning(&format!(
                        "NIC LEDs not applied ([led] nic_mode / night mode): {problem}"
                    ));
                    self.nic_led_problem = Some(problem);
                }
            }
            Ok(()) => {
                if self.nic_led_problem.take().is_some() {
                    crate::syslog::notice("NIC LEDs applied normally again");
                }
            }
        }
    }

    /// (name, health) per pool, from the last `zpool list`.
    fn pool_healths(&self) -> Vec<(String, String)> {
        self.pools
            .iter()
            .flatten()
            .map(|p| (p.name.clone(), p.health.clone()))
            .collect()
    }

    /// Bay LED state per SATA bay, from the last disk scan.
    pub fn bay_states(&self) -> Vec<(u32, led::BayState)> {
        hal::bay_led_states(&self.disks)
    }

    fn update_health_leds(&mut self) {
        let bay_states = self.bay_states();
        for &(bay, _) in &bay_states {
            self.apply_bay_led(bay);
        }
        self.monitor.check_bays(&bay_states);
        self.recompute_status_led();
    }

    /// What a bay's LEDs should show right now, by precedence:
    ///
    /// 1. an active `LOCATE` -- temporary, and explicitly asked for by
    ///    someone standing at the rack; the drive they're looking for is
    ///    often the failed one, so it has to win over `Failed` too;
    /// 2. an active error/critical override's `Alert` for this bay;
    /// 3. the SMART-derived health state.
    ///
    /// Recomputed from all three every time rather than remembered, so
    /// ending a layer exposes the *current* state of the ones below it,
    /// never a stale snapshot from when it started.
    fn bay_led_state(&self, bay: u32) -> led::BayState {
        if self.locate_bays.contains_key(&bay) {
            led::BayState::Locate
        } else if self.alert_bay == Some(bay) {
            led::BayState::Alert
        } else {
            self.bay_states()
                .into_iter()
                .find(|&(b, _)| b == bay)
                .map_or(led::BayState::Normal, |(_, s)| s)
        }
    }

    /// Writes `bay_led_state` to that bay's LEDs -- in its night-mode form
    /// while asleep, so ending a locate at 3am goes back to dark, not to
    /// daytime activity blinking.
    fn apply_bay_led(&self, bay: u32) {
        let state = self.bay_led_state(bay);
        if self.sleeping {
            led::set_bay_night(bay, state);
        } else {
            led::set_bay(bay, state);
        }
    }

    /// Called whenever the active override changes: starts/stops that
    /// bay's alert flash (a cheap, single-bay write -- not a full SMART
    /// re-scan) and recomputes the status LED to match. A bay that stops
    /// alerting goes back to whatever `bay_led_state` now says (its health
    /// state, or a locate), not blindly to `Normal`.
    fn sync_leds_to_override(&mut self) {
        let want = self
            .over
            .as_ref()
            .filter(|o| o.level.always_visible())
            .and_then(|o| o.bay);
        let previous = std::mem::replace(&mut self.alert_bay, want);
        if let Some(old) = previous.filter(|&b| Some(b) != want) {
            self.apply_bay_led(old);
        }
        if let Some(bay) = want {
            self.apply_bay_led(bay);
        }
        self.recompute_status_led();
    }

    /// The single "is anything wrong" indicator: pool health (factory
    /// patterns, unchanged), plus everything `HealthMonitor`/
    /// `FanController` track -- temps, fan health, SMART, and monitored
    /// NIC links -- folded into one severity via `Level`. Found live that
    /// this was needed: the LED previously only ever reflected pool
    /// health + a too-broad network check, so it could show amber (or
    /// miss a real problem) while completely disconnected from what the
    /// rest of the daemon was actually observing.
    ///
    /// Two things sit above that verdict: a chassis `LOCATE` (for its
    /// bounded TTL; the verdict is recomputed fresh the moment it ends) and
    /// night mode, which keeps the LED dark (as `led::enter_night_mode`
    /// left it) even when something like a `CLEAR` asks for a recompute --
    /// unless `[led] night_brightness` dims it instead, in which case it
    /// keeps showing the verdict.
    fn recompute_status_led(&self) {
        if self.locate_chassis.is_some() {
            led::set_status(led::StatusPattern::Locate);
        } else if self.sleeping && self.night_darkens_front() {
            led::set_status(led::StatusPattern::Off);
        } else if let Some(ov) = &self.over
            && let Some(pattern) = led::pattern_for_level(ov.level)
        {
            led::set_status(pattern);
        } else {
            led::set_status(self.health_summary().pattern);
        }
    }

    /// The power LED: flashing during a chassis `LOCATE`, otherwise dark at
    /// night and solid blue by day.
    fn apply_power_led(&self) {
        led::set_power(if self.locate_chassis.is_some() {
            led::PowerPattern::Locate
        } else if self.sleeping && self.night_darkens_front() {
            led::PowerPattern::Off
        } else {
            led::PowerPattern::On
        });
    }

    /// Re-applies every LED an active locate owns. Entering and leaving
    /// night mode both rewrite the front LEDs wholesale, and a locate is
    /// meant to keep blinking straight through either.
    fn reassert_locate_leds(&self) {
        for &bay in self.locate_bays.keys() {
            led::set_bay(bay, led::BayState::Locate);
        }
        if self.locate_chassis.is_some() {
            led::set_power(led::PowerPattern::Locate);
            led::set_status(led::StatusPattern::Locate);
        }
    }

    /// Starts a locate, or restarts the TTL of one already running.
    /// Deliberately exempt from the rules `show_override` enforces for
    /// messages: it never wakes the panel from night mode (it lights its
    /// own LEDs regardless, then hands them back to the night state), and
    /// it isn't held back while the action menu is open -- its LEDs apply
    /// at once, and the chassis screen just waits behind the menu in
    /// `render`.
    fn start_locate(&mut self, bay: Option<u32>, ttl_secs: u64) {
        // A TTL too large to represent (checked_add -> None) is as good as
        // "until LOCATE off" -- and must not panic the daemon.
        let until = (ttl_secs > 0)
            .then(|| Instant::now().checked_add(Duration::from_secs(ttl_secs)))
            .flatten();
        match bay {
            Some(bay) => {
                self.locate_bays.insert(bay, until);
                self.apply_bay_led(bay);
            }
            None => {
                self.locate_chassis = Some(ChassisLocate {
                    until,
                    hostname: hal::hostname().unwrap_or_default(),
                });
                self.reset_scroll();
                self.apply_power_led();
                self.recompute_status_led();
                self.apply_front_brightness();
            }
        }
    }

    /// `LOCATE off`: ends the locate on `bay`, or with `None`, every
    /// active locate, chassis included.
    fn stop_locate(&mut self, bay: Option<u32>) {
        match bay {
            Some(bay) => self.end_bay_locate(bay),
            None => {
                let bays: Vec<u32> = self.locate_bays.keys().copied().collect();
                for bay in bays {
                    self.end_bay_locate(bay);
                }
                self.end_chassis_locate();
            }
        }
    }

    /// Ends every locate whose TTL has run out.
    fn expire_locates(&mut self) {
        let now = Instant::now();
        let due = |until: Option<Instant>| until.is_some_and(|t| now >= t);
        let expired: Vec<u32> = self
            .locate_bays
            .iter()
            .filter(|&(_, &until)| due(until))
            .map(|(&bay, _)| bay)
            .collect();
        for bay in expired {
            self.end_bay_locate(bay);
        }
        if self.locate_chassis.as_ref().is_some_and(|c| due(c.until)) {
            self.end_chassis_locate();
        }
    }

    /// Hands a bay's LEDs back to whatever `bay_led_state` says now.
    fn end_bay_locate(&mut self, bay: u32) {
        if self.locate_bays.remove(&bay).is_some() {
            self.apply_bay_led(bay);
        }
    }

    fn end_chassis_locate(&mut self) {
        if self.locate_chassis.take().is_some() {
            self.reset_scroll();
            self.apply_power_led();
            self.recompute_status_led();
            self.apply_front_brightness();
        }
    }

    /// The same aggregate `recompute_status_led` turns into an LED
    /// pattern, plus the individual contributing pieces -- shared with
    /// `report::build` (the `status` socket request) so both use exactly
    /// this computation, not two copies that could drift. Deliberately
    /// does *not* factor in an active override (`self.over`) the way
    /// `recompute_status_led` does -- an override is what's currently
    /// being *shown*, this is the health computation underneath it,
    /// which the report labels separately (see `override_summary`).
    pub fn health_summary(&self) -> HealthSummary {
        use led::StatusPattern;

        let pool_healths = self.pool_healths();
        let pool_degraded = pool_healths.iter().any(|(_, h)| h == "DEGRADED");
        let pool_faulted = pool_healths
            .iter()
            .any(|(_, h)| matches!(h.as_str(), "FAULTED" | "UNAVAIL" | "OFFLINE"));
        let bay_failed = self.monitor.any_bay_failed();
        let temp_level = self.monitor.worst_temp_level();
        let network_level = self.network_health_level();

        // Worst of: this fan's health (pushed in from main.rs each tick,
        // since FanControllers live outside AppState), every currently-
        // connected temp sensor, monitored NIC link state, and whether any
        // bay is a confirmed SMART failure (Error -- a failed drive is
        // serious, same tier as a solid-red pool fault, even though it
        // doesn't necessarily mean the pool itself has degraded yet).
        let general = [
            self.fan_health,
            temp_level,
            network_level,
            if bay_failed {
                Level::Error
            } else {
                Level::Info
            },
        ]
        .into_iter()
        .max()
        .unwrap_or(Level::Info);

        let pattern = if general == Level::Critical {
            StatusPattern::CriticalFlashing
        } else if pool_faulted || general == Level::Error {
            StatusPattern::Failed
        } else if pool_degraded {
            // Factory-documented RAID-degraded pattern takes this slot
            // specifically (between Error and Warn) as long as nothing
            // worse is also true -- kept distinguishable from the generic
            // Warning amber above.
            StatusPattern::Degraded
        } else if general == Level::Warn {
            StatusPattern::Warning
        } else {
            StatusPattern::Ok
        };

        HealthSummary {
            pool_healths,
            pool_degraded,
            pool_faulted,
            bay_failed,
            temp_level,
            network_level,
            fan_health: self.fan_health,
            overall: general,
            pattern,
        }
    }

    /// Human-readable description of the active socket override, if any --
    /// for the `status` report. `None` when nothing's overriding the
    /// health-derived display.
    pub fn override_summary(&self) -> Option<String> {
        self.over.as_ref().map(|o| {
            let bay = o.bay.map(|b| format!(" bay={b}")).unwrap_or_default();
            format!("{:?}{bay}: {} / {}", o.level, o.line0, o.line1)
        })
    }

    /// Every active locate and how long it has left, for the `status`
    /// report. `None` when nothing is being located.
    pub fn locate_summary(&self) -> Option<String> {
        let left = |until: Option<Instant>| {
            until.map_or_else(
                || "until LOCATE off".to_string(),
                |t| format!("{}s left", secs_left(t)),
            )
        };
        let chassis = self
            .locate_chassis
            .iter()
            .map(|c| format!("chassis ({})", left(c.until)));
        let bays = self
            .locate_bays
            .iter()
            .map(|(bay, &until)| format!("bay {bay} ({})", left(until)));
        let all: Vec<String> = chassis.chain(bays).collect();
        (!all.is_empty()).then(|| all.join(", "))
    }

    /// Network's contribution to the aggregate above -- see
    /// `config::NetworkConfig`. Monitors either the explicit
    /// `monitored_nics` list, or (default, empty list) whatever
    /// `hal::configured_nics()` currently returns, so it auto-adjusts as
    /// interfaces are configured/unconfigured rather than needing the
    /// config updated to match.
    fn network_health_level(&self) -> Level {
        let net = &self.cfg.network;
        if !net.enabled {
            return Level::Info;
        }
        let monitored = hal::monitored_nics(net);
        if monitored.is_empty() {
            return Level::Info; // nothing configured/in-service to check
        }
        let down = monitored.iter().filter(|i| hal::link_is_down(i)).count();
        if down == 0 {
            Level::Info
        } else if down == monitored.len() {
            net.all_down_level.into()
        } else {
            net.some_down_level.into()
        }
    }

    /// Tells the state machine what the sleep schedule currently wants.
    /// Called every tick from `main`; actual sleep/wake transitions happen
    /// inside `tick()`, which also accounts for a recent manual wake and
    /// any active critical override before honoring it.
    pub fn set_schedule_sleep_wanted(&mut self, wanted: bool) {
        self.schedule_wants_sleep = wanted;
    }

    /// Re-fetches every category regardless of its refresh floor -- used
    /// only when data must be guaranteed fresh right now (startup, waking
    /// from sleep). Everything else should go through `refresh_stale`.
    pub fn refresh_all(&mut self) {
        self.network_cache.last_refresh = None;
        self.pools_cache.last_refresh = None;
        self.hdd_cache.last_refresh = None;
        self.temperature_cache.last_refresh = None;
        self.docker_cache.last_refresh = None;
        self.refresh_stale();
    }

    /// Called every tick. Re-fetches only the categories whose refresh
    /// floor has elapsed -- this is the actual refresh-on-display
    /// mechanism, not a fixed background poll: a category sitting outside
    /// the current rotation still respects its floor here, but in
    /// practice only gets looked at again once the rotation (or a manual
    /// page) is about to show it, since that's what drives how often
    /// `tick` even runs this check against a meaningfully later clock.
    pub fn refresh_stale(&mut self) {
        let r = &self.cfg.refresh;
        let pools_due = self
            .pools_cache
            .stale(Duration::from_secs(r.pools_min_secs));
        let hdd_due = self.hdd_cache.stale(Duration::from_secs(r.hdd_min_secs));

        if self.cfg.screens.network
            && self
                .network_cache
                .stale(Duration::from_secs(r.network_min_secs))
        {
            self.network_cache.screens = hal::network(&self.cfg.templates.network);
            self.network_cache.last_refresh = Some(Instant::now());
        }
        // Pool/hdd data (and so, health monitoring + LED updates) is
        // refreshed regardless of `cfg.screens.pools`/`.hdd` -- those only
        // gate the *display* screen. Alerting has no business being
        // silently disabled because someone turned off an LCD screen.
        if pools_due {
            self.pools = hal::pools();
            self.monitor.check_pools(&self.pool_healths());
            if self.cfg.screens.pools {
                self.pools_cache.screens =
                    hal::pool_screens(&self.cfg.templates.pool, self.pools.as_deref());
            }
            self.pools_cache.last_refresh = Some(Instant::now());
        }
        // One SMART scan serves both the HDD screens and the bay LEDs, on
        // whichever of the two cadences comes due first.
        if pools_due || hdd_due {
            self.disks = hal::disks();
        }
        if hdd_due {
            if self.cfg.screens.hdd {
                self.hdd_cache.screens = hal::hdd(&self.cfg, &self.disks);
            }
            self.hdd_cache.last_refresh = Some(Instant::now());
        }
        if self.cfg.screens.temperature
            && self
                .temperature_cache
                .stale(Duration::from_secs(r.temperature_min_secs))
        {
            self.temperature_cache.screens = hal::cpu_and_fan(&self.cfg);
            self.temperature_cache.last_refresh = Some(Instant::now());
        }
        // Independent of the temperature screen/cache above -- see
        // HealthMonitor::maybe_check_temps.
        self.monitor.maybe_check_temps(&self.cfg);
        if self.cfg.screens.docker
            && self
                .docker_cache
                .stale(Duration::from_secs(r.docker_min_secs))
        {
            self.docker_cache.screens =
                hal::docker_issues(&self.cfg.docker.ignore, &self.cfg.templates.docker);
            self.docker_cache.last_refresh = Some(Instant::now());
        }

        let mut screens = Vec::new();
        for category in self.cfg.screens.effective_order() {
            let cache = match category {
                Category::Network => &self.network_cache,
                Category::Pools => &self.pools_cache,
                Category::Hdd => &self.hdd_cache,
                Category::Temperature => &self.temperature_cache,
                Category::Docker => &self.docker_cache,
            };
            screens.extend(cache.screens.iter().cloned());
        }
        if screens.is_empty() {
            screens.push(Screen {
                line0: "LCM-STATUS".into(),
                line1: "no screens enabled".into(),
            });
        }
        if self.index >= screens.len() {
            self.index = 0;
        }
        self.screens = screens;

        // Bay/status LEDs are driven by pool + SMART health, so only worth
        // recomputing when that data actually changed -- not every tick.
        if pools_due || hdd_due {
            self.update_health_leds();
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Effect {
        if self.sleeping {
            // Any key just wakes the panel; it is never also acted on.
            // Stays awake for one resume-after-inactivity period even
            // though the schedule still wants it asleep, so a groggy 2am
            // glance doesn't get plunged back into darkness mid-read.
            //
            self.awake_override_until =
                Some(Instant::now() + Duration::from_secs(self.cfg.rotation.resume_after_secs));
            self.wake();
            return self.render();
        }

        match &mut self.mode {
            Mode::Status => match key {
                Key::Up => {
                    self.page(false);
                    Effect::None
                }
                Key::Down => {
                    self.page(true);
                    Effect::None
                }
                Key::Enter => {
                    let options = vec![Action::Shutdown, Action::Restart];
                    self.mode = Mode::ActionMenu {
                        options,
                        selection: 0,
                    };
                    Effect::None
                }
                _ => Effect::None,
            },
            Mode::ActionMenu { options, selection } => match key {
                Key::Up => {
                    *selection = selection.checked_sub(1).unwrap_or(options.len() - 1);
                    Effect::None
                }
                Key::Down => {
                    *selection = (*selection + 1) % options.len();
                    Effect::None
                }
                Key::Enter => {
                    let action = options[*selection];
                    self.mode = Mode::Confirm {
                        action,
                        deadline: Instant::now()
                            + Duration::from_secs(self.cfg.menu.confirm_timeout_secs),
                    };
                    Effect::None
                }
                Key::Back => {
                    self.mode = Mode::Status;
                    self.apply_pending_override();
                    Effect::None
                }
                _ => Effect::None,
            },
            Mode::Confirm { action, .. } => match key {
                Key::Enter => {
                    let action = *action;
                    self.mode = Mode::Status;
                    self.apply_pending_override();
                    Effect::RunAction(action)
                }
                Key::Back => {
                    self.mode = Mode::Status;
                    self.apply_pending_override();
                    Effect::None
                }
                _ => Effect::None,
            },
            // Any button at all, so whoever is standing at the box can stop
            // it without having to know which one.
            Mode::Countdown { action, label, .. } => {
                crate::syslog::notice(&format!(
                    "power schedule: {label}: cancelled from the front panel"
                ));
                let line0 = action.label().to_string();
                self.mode = Mode::Status;
                self.apply_pending_override();
                self.show_override(Override {
                    level: Level::Info,
                    expires_at: Some(Instant::now() + Duration::from_secs(5)),
                    bay: None,
                    line0,
                    line1: "CANCELLED".into(),
                });
                self.render()
            }
        }
    }

    /// A `[[power_schedule]]` shutdown/restart came due (`power::Scheduler`):
    /// counts down on the panel for `secs` -- any button cancels -- and
    /// then `tick` hands `action` back to the event loop to run. Takes
    /// over from the action menu or a confirm screen, and wakes the panel
    /// from night mode, so it can't go unseen. If a countdown is already
    /// running it's kept, except that a shutdown replaces a restart.
    pub fn start_countdown(&mut self, action: Action, secs: u64, label: &str) {
        if let Mode::Countdown {
            action: running, ..
        } = &mut self.mode
        {
            if action == Action::Shutdown && *running == Action::Restart {
                *running = Action::Shutdown;
            }
            return;
        }
        if self.sleeping {
            self.wake();
        }
        self.mode = Mode::Countdown {
            action,
            deadline: Instant::now() + Duration::from_secs(secs),
            label: label.to_string(),
        };
        self.reset_scroll();
    }

    /// The running power-schedule countdown, if any -- for the `status`
    /// report.
    pub fn countdown_summary(&self) -> Option<String> {
        match &self.mode {
            Mode::Countdown {
                action,
                deadline,
                label,
            } => Some(format!(
                "{} in {}s ({label}; any front-panel button cancels)",
                action.label(),
                deadline.saturating_duration_since(Instant::now()).as_secs()
            )),
            _ => None,
        }
    }

    /// Manual UP (`forward = false`) / DOWN paging, wrapping at both ends.
    fn page(&mut self, forward: bool) {
        let len = self.screens.len();
        if len == 0 {
            return;
        }
        self.index = if forward {
            (self.index + 1) % len
        } else {
            (self.index + len - 1) % len
        };
        self.auto_rotate = false;
        self.resume_at =
            Some(Instant::now() + Duration::from_secs(self.cfg.rotation.resume_after_secs));
        self.last_dwell = Instant::now();
        self.reset_scroll();
    }

    fn reset_scroll(&mut self) {
        self.scroll0 = Scroll::default();
        self.scroll1 = Scroll::default();
    }

    pub fn apply_socket_command(&mut self, cmd: SocketCommand) {
        match cmd {
            SocketCommand::Clear { bay } => {
                // A bare CLEAR (no bay) drops the whole override; CLEAR
                // bay=N only makes sense when it matches the active
                // override's bay, otherwise there's nothing to do.
                if bay.is_none() || bay == self.over.as_ref().and_then(|o| o.bay) {
                    self.over = None;
                    self.sync_leds_to_override();
                }
            }
            SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0,
                line1,
            } => {
                // error/critical are meant to always be seen: force them to
                // persist regardless of whatever ttl the caller passed. A
                // ttl too large to represent as an `Instant` persists too,
                // rather than panicking the daemon.
                let expires_at = if level.always_visible() || ttl_secs == 0 {
                    None
                } else {
                    Instant::now().checked_add(Duration::from_secs(ttl_secs))
                };
                let ov = Override {
                    level,
                    expires_at,
                    bay,
                    line0,
                    line1,
                };

                if matches!(self.mode, Mode::Status) {
                    self.show_override(ov);
                } else if self
                    .pending_over
                    .as_ref()
                    .is_none_or(|p| ov.level >= p.level)
                {
                    // Never interrupt the action menu / confirm flow; hold
                    // the most severe message until it closes.
                    self.pending_over = Some(ov);
                }
            }
            SocketCommand::Locate { bay, ttl_secs } => self.start_locate(bay, ttl_secs),
            SocketCommand::LocateOff { bay } => self.stop_locate(bay),
            // Handled directly in main.rs's loop (needs the fan status,
            // which lives outside AppState) before a command ever reaches
            // here -- never actually matched at runtime, just keeps this
            // exhaustive.
            SocketCommand::StatusRequest(_) => {}
        }
    }

    fn apply_pending_override(&mut self) {
        if let Some(ov) = self.pending_over.take() {
            self.show_override(ov);
        }
    }

    /// The one place an override becomes active, so every path (socket,
    /// or a message held while the menu was open) obeys the same rules: a
    /// higher (or equal) level can replace what's showing, a lower one
    /// never steps on a more severe active alert; while asleep, routine
    /// messages are dropped and error/critical wake the panel.
    fn show_override(&mut self, ov: Override) {
        if self.over.as_ref().is_some_and(|o| ov.level < o.level) {
            return;
        }
        if self.sleeping {
            if !ov.level.always_visible() {
                return;
            }
            self.wake();
        }
        self.over = Some(ov);
        self.reset_scroll();
        self.sync_leds_to_override();
    }

    /// Leaves night mode: every LED night mode darkened (Power LED, LAN
    /// rail, NIC ports, bays, status) is restored, and the screens are
    /// re-fetched since they may be hours stale. Shared by every way the
    /// panel wakes -- schedule, button, and an urgent socket alert -- so
    /// none of them can restore only some of the LEDs.
    fn wake(&mut self) {
        self.sleeping = false;
        let result = led::exit_night_mode(self.cfg.led.nic_mode, self.night_darkens_front());
        self.note_nic_leds(result);
        self.apply_front_brightness();
        self.refresh_all();
        self.reassert_locate_leds();
    }

    /// Called on every event-loop wakeup; returns what to display or do.
    pub fn tick(&mut self) -> Effect {
        // Ahead of the asleep early-returns below: a locate started at
        // night still has to end on time.
        self.expire_locates();

        // Sleep/wake transitions take priority over everything else, but
        // never fire mid-menu-interaction, never re-sleep through an active
        // override (e.g. a critical alert still showing), and respect the
        // post-manual-wake grace period.
        let grace_active = self
            .awake_override_until
            .is_some_and(|t| Instant::now() < t);
        if !grace_active {
            self.awake_override_until = None;
        }

        if self.schedule_wants_sleep
            && !self.sleeping
            && !grace_active
            && self.over.is_none()
            && matches!(self.mode, Mode::Status)
        {
            self.sleeping = true;
            let result = led::enter_night_mode(self.night_darkens_front());
            self.note_nic_leds(result);
            self.apply_front_brightness();
            // Night mode just darkened every LED wholesale; a locate in
            // progress keeps blinking straight through it.
            self.reassert_locate_leds();
        }
        if !self.schedule_wants_sleep && self.sleeping {
            self.wake();
            return self.render();
        }
        if self.sleeping {
            // Blanks the text but leaves the panel's own MCU powered --
            // unlike cutting power:lcd, which also kills the MCU (and so,
            // its ability to report a button press at all: confirmed live,
            // zero serial frames arrive while power:lcd is 0). This is the
            // whole point: night mode has to stay wakeable by a button.
            // A chassis locate is the one thing shown anyway, without
            // waking the rest of the panel.
            return if self.locate_chassis.is_some() {
                self.render()
            } else {
                Effect::Render(String::new(), String::new())
            };
        }

        // Checked before `refresh_stale`, which can block on slow
        // subprocesses, so a shutdown isn't held up behind a SMART scan.
        if let Mode::Countdown {
            action,
            deadline,
            label,
        } = &self.mode
            && Instant::now() >= *deadline
        {
            let action = *action;
            crate::syslog::warning(&format!(
                "power schedule: {label}: countdown over, running {}",
                action.label().to_ascii_lowercase()
            ));
            self.mode = Mode::Status;
            self.apply_pending_override();
            return Effect::RunAction(action);
        }

        self.refresh_stale();

        // Expire a timed-out override.
        if let Some(ov) = &self.over
            && let Some(exp) = ov.expires_at
            && Instant::now() >= exp
        {
            self.over = None;
            self.reset_scroll();
            self.sync_leds_to_override();
        }

        // Resume auto-rotation after manual paging goes idle.
        if !self.auto_rotate
            && let Some(resume_at) = self.resume_at
            && Instant::now() >= resume_at
        {
            self.auto_rotate = true;
            self.resume_at = None;
        }

        // Auto-cancel a stale confirm screen.
        if let Mode::Confirm { deadline, .. } = &self.mode
            && Instant::now() >= *deadline
        {
            self.mode = Mode::Status;
            self.apply_pending_override();
        }

        // Advance rotation if it's this screen's turn to change and nothing
        // is currently scrolling (don't cut a scroll cycle short).
        if matches!(self.mode, Mode::Status)
            && self.over.is_none()
            && self.locate_chassis.is_none()
            && self.auto_rotate
            && !self.screens.is_empty()
            && !self.is_scrolling()
            && self.last_dwell.elapsed() >= Duration::from_secs(self.cfg.rotation.dwell_secs)
        {
            self.index = (self.index + 1) % self.screens.len();
            self.last_dwell = Instant::now();
            self.reset_scroll();
        }

        self.render()
    }

    /// True while a line is overflowing 16 chars and hasn't finished at
    /// least one full scroll pass yet -- lets rotation wait for a first
    /// readable pass instead of cutting it off, without blocking forever.
    fn is_scrolling(&self) -> bool {
        let pending = |s: &Scroll| s.text.chars().count() > 16 && !s.completed_a_pass;
        pending(&self.scroll0) || pending(&self.scroll1)
    }

    fn render(&mut self) -> Effect {
        if self.sleeping && self.locate_chassis.is_none() {
            return Effect::None;
        }

        let (raw0, raw1) = match &self.mode {
            Mode::Status => {
                // A chassis locate outranks even a critical override on
                // the LCD, for the same bounded-TTL reason it does on the
                // status LED -- the override is still there, untouched,
                // and shows again the moment the locate ends.
                if let Some(locate) = &self.locate_chassis {
                    locate.screen()
                } else if let Some(ov) = &self.over {
                    (ov.line0.clone(), ov.line1.clone())
                } else if let Some(s) = self.screens.get(self.index) {
                    (s.line0.clone(), s.line1.clone())
                } else {
                    (String::new(), String::new())
                }
            }
            Mode::ActionMenu { options, selection } => {
                let marker = |i: usize| if i == *selection { ">" } else { " " };
                let opt = |i: usize| options.get(i).map_or("", |a| a.label());
                (
                    format!("{}{}", marker(*selection), opt(*selection)),
                    "UP/DN ENTER BACK".to_string(),
                )
            }
            Mode::Confirm { action, .. } => (
                format!("CONFIRM {}?", action.label()),
                "ENTER=yes BACK=no".to_string(),
            ),
            Mode::Countdown {
                action, deadline, ..
            } => (
                format!(
                    "{} IN {}",
                    action.label(),
                    countdown_text(deadline.saturating_duration_since(Instant::now()))
                ),
                "ANY KEY: CANCEL".to_string(),
            ),
        };

        let cap = self.cfg.display.scroll_max_chars;
        let line0 = self.scroll_step(0, truncate(&raw0, cap));
        let line1 = self.scroll_step(1, truncate(&raw1, cap));
        Effect::Render(line0, line1)
    }

    fn scroll_step(&mut self, which: u8, text: String) -> String {
        let cfg = &self.cfg.display;
        let scroll = if which == 0 {
            &mut self.scroll0
        } else {
            &mut self.scroll1
        };

        if scroll.text != text {
            *scroll = Scroll {
                text,
                offset: 0,
                last_step: Some(Instant::now()),
                started: Some(Instant::now()),
                completed_a_pass: false,
            };
        }

        let chars: Vec<char> = scroll.text.chars().collect();
        if chars.len() <= 16 {
            return scroll.text.clone();
        }

        let now = Instant::now();
        let in_start_pause = scroll
            .started
            .is_some_and(|s| now.duration_since(s) < Duration::from_millis(cfg.scroll_pause_ms));

        if !in_start_pause
            && let Some(last) = scroll.last_step
            && now.duration_since(last) >= Duration::from_millis(cfg.scroll_step_ms)
        {
            scroll.offset += 1;
            scroll.last_step = Some(now);
            // Loop with a gap of spaces between the end and restart.
            let gap = cfg.scroll_gap;
            if scroll.offset > chars.len() + gap {
                scroll.offset = 0;
                scroll.started = Some(now); // pause again at the loop point
                scroll.completed_a_pass = true;
            }
        }

        let mut window = String::with_capacity(16);
        let padded_len = chars.len() + cfg.scroll_gap;
        for i in 0..16 {
            let pos = (scroll.offset + i) % padded_len;
            window.push(if pos < chars.len() { chars[pos] } else { ' ' });
        }
        window
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Time left on a countdown, rounded up so it never shows 0 while there's
/// still time: "0:42", or whole minutes ("15m") from 10 minutes up, so
/// "SHUTDOWN IN ..." always fits on one 16-char line without scrolling.
fn countdown_text(left: Duration) -> String {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    if secs >= 600 {
        format!("{}m", secs.div_ceil(60))
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::led::test_writes;

    fn state() -> AppState {
        AppState::new(Config::default())
    }

    fn show(state: &mut AppState, level: Level, text: &str) {
        state.apply_socket_command(SocketCommand::Show {
            level,
            ttl_secs: 0,
            bay: None,
            line0: text.to_string(),
            line1: String::new(),
        });
    }

    fn active(state: &AppState) -> Option<(Level, &str)> {
        state.over.as_ref().map(|o| (o.level, o.line0.as_str()))
    }

    #[test]
    fn huge_show_ttl_persists_instead_of_panicking() {
        let mut s = state();
        s.apply_socket_command(SocketCommand::Show {
            level: Level::Info,
            ttl_secs: u64::MAX,
            bay: None,
            line0: "FOREVER".to_string(),
            line1: String::new(),
        });
        assert_eq!(active(&s), Some((Level::Info, "FOREVER")));
        assert!(s.over.as_ref().is_some_and(|o| o.expires_at.is_none()));
    }

    #[test]
    fn lower_level_never_replaces_a_more_severe_alert() {
        let mut s = state();
        show(&mut s, Level::Critical, "FIRE");
        show(&mut s, Level::Info, "hello");
        assert_eq!(active(&s), Some((Level::Critical, "FIRE")));
        show(&mut s, Level::Critical, "FIRE 2");
        assert_eq!(active(&s), Some((Level::Critical, "FIRE 2")));
    }

    #[test]
    fn message_held_during_menu_cannot_replace_a_more_severe_alert() {
        let mut s = state();
        show(&mut s, Level::Critical, "FIRE");
        s.handle_key(Key::Enter); // open the action menu
        show(&mut s, Level::Info, "hello");
        s.handle_key(Key::Back); // close it; held message is applied
        assert_eq!(active(&s), Some((Level::Critical, "FIRE")));
    }

    #[test]
    fn menu_holds_the_most_severe_message_not_the_latest() {
        let mut s = state();
        s.handle_key(Key::Enter);
        show(&mut s, Level::Error, "DISK");
        show(&mut s, Level::Info, "hello");
        assert!(active(&s).is_none(), "menu must not be interrupted");
        s.handle_key(Key::Back);
        assert_eq!(active(&s), Some((Level::Error, "DISK")));
    }

    #[test]
    fn urgent_alert_while_asleep_fully_exits_night_mode() {
        let mut s = state();
        s.sleeping = true;
        test_writes::take();
        show(&mut s, Level::Critical, "FIRE");
        assert!(!s.sleeping);
        assert_eq!(active(&s), Some((Level::Critical, "FIRE")));
        assert!(test_writes::wrote("blue:power", "brightness", "1"));
        assert!(test_writes::wrote("blue:lan", "brightness", "1"));
    }

    #[test]
    fn routine_message_while_asleep_is_dropped() {
        let mut s = state();
        s.sleeping = true;
        test_writes::take();
        show(&mut s, Level::Warn, "meh");
        assert!(s.sleeping);
        assert!(active(&s).is_none());
        assert!(!test_writes::wrote("blue:power", "brightness", "1"));
    }

    fn locate(state: &mut AppState, bay: Option<u32>, ttl_secs: u64) {
        state.apply_socket_command(SocketCommand::Locate { bay, ttl_secs });
    }

    fn locate_off(state: &mut AppState, bay: Option<u32>) {
        state.apply_socket_command(SocketCommand::LocateOff { bay });
    }

    /// Makes every active locate's TTL due, as if it had run out.
    fn run_out_locates(state: &mut AppState) {
        let now = Some(Instant::now());
        for until in state.locate_bays.values_mut() {
            *until = now;
        }
        if let Some(c) = &mut state.locate_chassis {
            c.until = now;
        }
        state.expire_locates();
    }

    fn disk(bay: u32, smart: hal::SmartStatus) -> hal::Disk {
        hal::Disk {
            name: format!("sd{bay}"),
            bay: Some(bay),
            smart,
        }
    }

    const LOCATE_MS: &str = "250";

    fn bay_is_locating(bay: u32) -> bool {
        [
            format!("sata{bay}:green:disk"),
            format!("sata{bay}:red:disk"),
        ]
        .iter()
        .all(|led| {
            test_writes::wrote(led, "trigger", "timer")
                && test_writes::wrote(led, "delay_on", LOCATE_MS)
                && test_writes::wrote(led, "delay_off", LOCATE_MS)
        })
    }

    #[test]
    fn locate_pattern_is_unlike_every_other_bay_state() {
        let pattern = |state: led::BayState| {
            test_writes::take();
            led::set_bay(1, state);
            test_writes::take()
        };
        let locate = pattern(led::BayState::Locate);
        for other in [
            led::BayState::Normal,
            led::BayState::Failed,
            led::BayState::Alert,
            led::BayState::Standby,
        ] {
            assert_ne!(pattern(other), locate, "{other:?}");
        }
        assert_eq!(led::LOCATE_BLINK_MS.to_string(), LOCATE_MS);
    }

    #[test]
    fn bay_locate_blinks_then_restores_health_state() {
        let mut s = state();
        s.disks = vec![disk(2, hal::SmartStatus::Standby)];
        test_writes::take();
        locate(&mut s, Some(2), 60);
        assert!(bay_is_locating(2));

        test_writes::take();
        run_out_locates(&mut s);
        assert!(s.locate_bays.is_empty());
        // Back to Standby's slow green blip, red off.
        assert!(test_writes::wrote("sata2:green:disk", "delay_off", "9750"));
        assert!(test_writes::wrote("sata2:red:disk", "brightness", "0"));
    }

    #[test]
    fn smart_failure_found_mid_locate_shows_once_it_ends() {
        let mut s = state();
        s.disks = vec![disk(3, hal::SmartStatus::Passed)];
        locate(&mut s, Some(3), 0);

        // A refresh finds the drive failed while it's being located: the
        // locate keeps the LEDs...
        s.disks = vec![disk(3, hal::SmartStatus::Failed)];
        test_writes::take();
        s.update_health_leds();
        assert!(bay_is_locating(3));
        assert!(!test_writes::wrote("sata3:red:disk", "brightness", "1"));

        // ...and the failure is what's left when it ends.
        test_writes::take();
        locate_off(&mut s, Some(3));
        assert!(test_writes::wrote("sata3:red:disk", "brightness", "1"));
        assert!(test_writes::wrote("sata3:green:disk", "brightness", "0"));
    }

    #[test]
    fn locate_outranks_an_override_alert_and_hands_back_to_it() {
        let mut s = state();
        s.apply_socket_command(SocketCommand::Show {
            level: Level::Critical,
            ttl_secs: 0,
            bay: Some(1),
            line0: "DISK".into(),
            line1: String::new(),
        });
        test_writes::take();
        locate(&mut s, Some(1), 60);
        assert!(bay_is_locating(1));

        // The override's own bookkeeping re-runs mid-locate (e.g. a newer
        // critical message) without stealing the LEDs back.
        test_writes::take();
        s.sync_leds_to_override();
        assert!(!test_writes::wrote("sata1:red:disk", "delay_on", "1000"));

        test_writes::take();
        run_out_locates(&mut s);
        assert!(test_writes::wrote("sata1:red:disk", "delay_on", "1000"));
    }

    #[test]
    fn locate_off_without_a_bay_stops_everything() {
        let mut s = state();
        locate(&mut s, Some(1), 0);
        locate(&mut s, Some(4), 0);
        locate(&mut s, None, 0);
        locate_off(&mut s, None);
        assert!(s.locate_bays.is_empty());
        assert!(s.locate_chassis.is_none());
    }

    #[test]
    fn locate_off_for_one_bay_leaves_the_rest() {
        let mut s = state();
        locate(&mut s, Some(1), 0);
        locate(&mut s, Some(2), 0);
        locate(&mut s, None, 0);
        locate_off(&mut s, Some(1));
        assert_eq!(s.locate_bays.keys().copied().collect::<Vec<_>>(), [2]);
        assert!(s.locate_chassis.is_some());
    }

    #[test]
    fn clear_does_not_cancel_a_locate() {
        let mut s = state();
        locate(&mut s, Some(2), 0);
        locate(&mut s, None, 0);
        s.apply_socket_command(SocketCommand::Clear { bay: None });
        s.apply_socket_command(SocketCommand::Clear { bay: Some(2) });
        assert!(s.locate_bays.contains_key(&2));
        assert!(s.locate_chassis.is_some());
    }

    #[test]
    fn absurd_locate_ttl_does_not_panic() {
        let mut s = state();
        locate(&mut s, Some(1), u64::MAX);
        assert_eq!(s.locate_bays.get(&1), Some(&None));
    }

    #[test]
    fn repeating_a_locate_restarts_its_ttl() {
        let mut s = state();
        locate(&mut s, Some(2), 5);
        locate(&mut s, Some(2), 0);
        assert_eq!(s.locate_bays.get(&2), Some(&None));
    }

    #[test]
    fn chassis_locate_shows_on_lcd_and_leds_then_restores() {
        let mut s = state();
        show(&mut s, Level::Critical, "FIRE");
        test_writes::take();
        locate(&mut s, None, 30);
        assert!(test_writes::wrote("blue:power", "delay_on", LOCATE_MS));
        assert!(test_writes::wrote("green:status", "delay_on", LOCATE_MS));
        assert!(test_writes::wrote("red:status", "delay_on", LOCATE_MS));
        let Effect::Render(_, line1) = s.render() else {
            panic!("expected a render");
        };
        assert_eq!(line1, "LOCATE 30s");

        test_writes::take();
        run_out_locates(&mut s);
        assert!(test_writes::wrote("blue:power", "brightness", "1"));
        // The critical override was underneath all along.
        assert!(test_writes::wrote("red:status", "delay_on", "1000"));
        let Effect::Render(line0, _) = s.render() else {
            panic!("expected a render");
        };
        assert_eq!(line0, "FIRE");
    }

    #[test]
    fn chassis_locate_never_interrupts_the_action_menu() {
        let mut s = state();
        s.handle_key(Key::Enter);
        locate(&mut s, None, 0);
        let Effect::Render(line0, _) = s.render() else {
            panic!("expected a render");
        };
        assert_eq!(line0, ">SHUTDOWN");
    }

    #[test]
    fn locate_at_night_lights_without_waking_then_goes_dark_again() {
        let mut s = state();
        s.sleeping = true;
        s.schedule_wants_sleep = true;
        s.disks = vec![disk(2, hal::SmartStatus::Passed)];
        test_writes::take();
        locate(&mut s, Some(2), 0);
        locate(&mut s, None, 0);
        assert!(s.sleeping, "a locate must not wake the panel");
        assert!(bay_is_locating(2));
        assert!(test_writes::wrote("blue:power", "delay_on", LOCATE_MS));
        assert!(!test_writes::wrote("blue:lan", "brightness", "1"));
        // The LCD shows the locate screen, nothing else.
        let Effect::Render(_, line1) = s.tick() else {
            panic!("expected a render");
        };
        assert_eq!(line1, "LOCATE");

        test_writes::take();
        locate_off(&mut s, None);
        // Night state, not daytime: green dark (no activity trigger),
        // power and status dark.
        assert!(test_writes::wrote("sata2:green:disk", "brightness", "0"));
        assert!(!test_writes::wrote(
            "sata2:green:disk",
            "trigger",
            "asustor-sata2"
        ));
        assert!(test_writes::wrote("blue:power", "brightness", "0"));
        assert!(test_writes::wrote("green:status", "brightness", "0"));
        assert!(matches!(s.tick(), Effect::Render(a, b) if a.is_empty() && b.is_empty()));
    }

    fn state_with(edit: impl FnOnce(&mut Config)) -> AppState {
        let mut cfg = Config::default();
        edit(&mut cfg);
        AppState::new(cfg)
    }

    /// The front LED brightness duty last written, if any.
    fn front_pwm_written() -> Option<String> {
        test_writes::take()
            .into_iter()
            .rev()
            .find(|(l, a, _)| l == "front-brightness" && a == "pwm")
            .map(|(_, _, v)| v)
    }

    #[test]
    fn lcd_goes_dark_at_night_unless_configured_or_locating() {
        let mut s = state();
        assert!(s.display_wanted());
        s.set_schedule_sleep_wanted(true);
        s.tick();
        assert!(s.sleeping);
        assert!(!s.display_wanted());
        locate(&mut s, None, 0);
        assert!(s.display_wanted(), "a chassis locate lights the LCD");
        locate_off(&mut s, None);
        assert!(!s.display_wanted());
        // Any key wakes it (and is not acted on).
        s.handle_key(Key::Down);
        assert!(s.display_wanted());

        let mut s = state_with(|c| c.sleep.lcd_off = false);
        s.set_schedule_sleep_wanted(true);
        s.tick();
        assert!(s.sleeping);
        assert!(s.display_wanted(), "lcd_off = false only blanks the text");
    }

    #[test]
    fn brightness_is_left_alone_unless_configured() {
        let mut s = state();
        test_writes::take();
        s.init_leds();
        s.set_schedule_sleep_wanted(true);
        s.tick();
        assert_eq!(front_pwm_written(), None);
    }

    #[test]
    fn day_brightness_applies_at_startup() {
        let mut s = state_with(|c| c.led.brightness = Some(30));
        test_writes::take();
        s.init_leds();
        assert_eq!(front_pwm_written().as_deref(), Some("179"));
    }

    #[test]
    fn night_brightness_dims_front_leds_instead_of_darkening_them() {
        let mut s = state_with(|c| {
            c.led.brightness = Some(100);
            c.led.night_brightness = Some(10);
        });
        s.init_leds();
        test_writes::take();
        s.set_schedule_sleep_wanted(true);
        s.tick();
        assert!(s.sleeping);
        let writes = test_writes::take();
        let wrote = |l: &str, a: &str, v: &str| {
            writes
                .iter()
                .any(|(wl, wa, wv)| wl == l && wa == a && wv == v)
        };
        assert!(wrote("front-brightness", "pwm", "230"));
        // Front LEDs keep showing; bay greens still go dark.
        assert!(!wrote("blue:power", "brightness", "0"));
        assert!(!wrote("green:status", "brightness", "0"));
        assert!(!wrote("blue:lan", "brightness", "0"));
        assert!(wrote("sata1:green:disk", "brightness", "0"));

        // A chassis locate is meant to be seen: full day brightness.
        locate(&mut s, None, 0);
        assert_eq!(front_pwm_written().as_deref(), Some("0"));
        locate_off(&mut s, None);
        assert_eq!(front_pwm_written().as_deref(), Some("230"));

        s.set_schedule_sleep_wanted(false);
        s.tick();
        assert!(!s.sleeping);
        assert_eq!(front_pwm_written().as_deref(), Some("0"));
    }

    #[test]
    fn night_brightness_alone_restores_the_bios_level_on_wake() {
        let mut s = state_with(|c| c.led.night_brightness = Some(0));
        s.init_leds(); // reads the BIOS level: 51 under cfg(test)
        s.set_schedule_sleep_wanted(true);
        s.tick();
        assert_eq!(front_pwm_written().as_deref(), Some("255"));
        s.set_schedule_sleep_wanted(false);
        s.tick();
        assert_eq!(front_pwm_written().as_deref(), Some("51"));
    }

    #[test]
    fn startup_turns_the_front_leds_back_on() {
        // As a previous run stopped mid-night-mode left them.
        let mut s = state();
        test_writes::take();
        s.init_leds();
        assert!(test_writes::wrote("blue:power", "brightness", "1"));
        assert!(test_writes::wrote("blue:lan", "brightness", "1"));
        assert!(test_writes::wrote(
            "green:usb",
            "trigger",
            "asustor-front-usb"
        ));
    }

    #[test]
    fn bay_mode_is_applied_only_when_configured() {
        let mut s = state();
        test_writes::take();
        s.init_leds();
        assert!(
            !test_writes::take()
                .iter()
                .any(|(l, _, _)| l == "disk_led_ready")
        );

        let mut s = state_with(|c| c.led.bay_mode = Some(led::BayLedMode::Activity));
        test_writes::take();
        s.init_leds();
        assert!(test_writes::wrote("disk_led_ready", "value", "N"));
    }

    #[test]
    fn entering_night_mode_keeps_a_locate_blinking() {
        let mut s = state();
        locate(&mut s, Some(1), 0);
        s.set_schedule_sleep_wanted(true);
        test_writes::take();
        s.tick();
        assert!(s.sleeping);
        // Night mode darkened sata1's green; the locate put it straight back.
        let writes = test_writes::take();
        let last_green_trigger = writes
            .iter()
            .rev()
            .find(|(l, a, _)| l == "sata1:green:disk" && a == "trigger")
            .map(|(_, _, v)| v.as_str());
        assert_eq!(last_green_trigger, Some("timer"));
    }

    #[test]
    fn locate_summary_lists_what_is_active() {
        let mut s = state();
        assert_eq!(s.locate_summary(), None);
        locate(&mut s, Some(3), 0);
        locate(&mut s, None, 0);
        assert_eq!(
            s.locate_summary().as_deref(),
            Some("chassis (until LOCATE off), bay 3 (until LOCATE off)")
        );
    }

    fn lines(effect: Effect) -> (String, String) {
        match effect {
            Effect::Render(l0, l1) => (l0, l1),
            _ => panic!("expected a render"),
        }
    }

    #[test]
    fn countdown_shows_then_runs_the_action() {
        let mut s = state();
        s.start_countdown(Action::Shutdown, 60, "#1 test");
        let (l0, l1) = lines(s.render());
        assert_eq!(l0, "SHUTDOWN IN 1:00");
        assert_eq!(l1, "ANY KEY: CANCEL");
        assert!(s.countdown_summary().is_some());

        let mut s = state();
        s.start_countdown(Action::Restart, 0, "#1 test");
        assert!(matches!(s.tick(), Effect::RunAction(Action::Restart)));
        assert!(matches!(s.mode, Mode::Status));
    }

    #[test]
    fn any_key_cancels_a_countdown() {
        for key in [Key::Up, Key::Down, Key::Back, Key::Enter, Key::Wake] {
            let mut s = state();
            s.start_countdown(Action::Shutdown, 60, "#1 test");
            assert!(!matches!(s.handle_key(key), Effect::RunAction(_)));
            assert!(matches!(s.mode, Mode::Status), "{key:?}");
            assert_eq!(active(&s), Some((Level::Info, "SHUTDOWN")));
        }
    }

    #[test]
    fn countdown_takes_over_the_menu_and_holds_socket_messages() {
        let mut s = state();
        s.handle_key(Key::Enter); // action menu open
        s.start_countdown(Action::Restart, 60, "#1 test");
        assert!(matches!(s.mode, Mode::Countdown { .. }));
        show(&mut s, Level::Error, "DISK");
        assert!(active(&s).is_none(), "countdown must not be interrupted");
        // A shutdown due meanwhile upgrades it; nothing downgrades it.
        s.start_countdown(Action::Shutdown, 600, "#2 test");
        s.start_countdown(Action::Restart, 600, "#3 test");
        assert!(matches!(
            s.mode,
            Mode::Countdown {
                action: Action::Shutdown,
                ..
            }
        ));
        s.handle_key(Key::Back);
        assert_eq!(active(&s), Some((Level::Error, "DISK")));
    }

    #[test]
    fn countdown_wakes_the_panel_and_keeps_it_awake() {
        let mut s = state();
        s.sleeping = true;
        s.set_schedule_sleep_wanted(true);
        s.start_countdown(Action::Shutdown, 60, "#1 test");
        assert!(!s.sleeping);
        let (l0, _) = lines(s.tick());
        assert!(l0.starts_with("SHUTDOWN IN"), "{l0}");
        assert!(!s.sleeping);
    }

    #[test]
    fn countdown_text_fits_the_panel() {
        assert_eq!(countdown_text(Duration::from_millis(41_200)), "0:42");
        assert_eq!(countdown_text(Duration::from_secs(599)), "9:59");
        assert_eq!(countdown_text(Duration::from_secs(3600)), "60m");
        assert!(format!("SHUTDOWN IN {}", countdown_text(Duration::from_secs(599))).len() <= 16);
    }

    #[test]
    fn button_wake_exits_night_mode_without_acting_on_the_key() {
        let mut s = state();
        s.sleeping = true;
        test_writes::take();
        s.handle_key(Key::Enter);
        assert!(!s.sleeping);
        assert!(
            matches!(s.mode, Mode::Status),
            "wake key must not open the menu"
        );
        assert!(test_writes::wrote("blue:power", "brightness", "1"));
    }
}
