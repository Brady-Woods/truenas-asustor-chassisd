//! The daemon's state machine: status rotation, the action menu, confirm
//! flow, socket overrides, and sleep -- all in one place so the
//! interactions between them (e.g. "a critical alert can preempt rotation
//! but not a confirm screen") are enforced in one spot, not scattered
//! across the event loop.

use crate::config::Config;
use crate::hal::{self, Screen};
use crate::protocol::Key;
use crate::led;
use crate::socket::{Level, SocketCommand};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Shutdown,
    Restart,
    Eject,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Shutdown => "SHUTDOWN",
            Action::Restart => "RESTART",
            Action::Eject => "EJECT USB",
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

#[derive(Debug)]
enum Mode {
    /// Normal operation: paging/rotating through gathered screens.
    Status,
    /// ENTER was pressed from Status; choosing an action.
    ActionMenu { options: Vec<Action>, selection: usize },
    /// An action was selected; awaiting confirm/cancel.
    Confirm { action: Action, deadline: Instant },
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
        self.last_refresh.map(|t| t.elapsed() >= floor).unwrap_or(true)
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
    sleeping: bool,
    schedule_wants_sleep: bool,
    awake_override_until: Option<Instant>,
    scroll0: Scroll,
    scroll1: Scroll,
    eject_available: bool,
    monitor: crate::monitor::HealthMonitor,
    /// Worst current fan health across every configured fan -- pushed in
    /// each tick from main.rs, which owns the actual `FanController`s (a
    /// separate top-level value from `AppState`, alongside it in the event
    /// loop, not inside it). Same "current state, not transition-gated"
    /// reasoning as `HealthMonitor`'s own LED-facing accessors: this feeds
    /// `recompute_status_led`, syslog transitions are logged by
    /// `FanController` itself.
    fan_health: Level,
}

/// Everything that feeds the status LED, plus the resulting verdict --
/// see `AppState::health_summary`. `pub(crate)` since this is an
/// implementation detail shared with `report.rs`, not part of the crate's
/// (nonexistent) public API.
pub(crate) struct HealthSummary {
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
    SetLcdPower(bool),
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
            sleeping: false,
            schedule_wants_sleep: false,
            awake_override_until: None,
            scroll0: Scroll::default(),
            scroll1: Scroll::default(),
            eject_available: false,
            monitor: crate::monitor::HealthMonitor::new(),
            fan_health: Level::Info,
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
        for iface in hal::physical_nics() {
            led::set_nic_mode(&iface, self.cfg.led.nic_mode);
        }
        self.update_health_leds();
    }

    fn update_health_leds(&mut self) {
        let bay_states = hal::bay_led_states();
        for &(bay, bay_state) in &bay_states {
            led::set_bay(bay, bay_state);
        }
        self.monitor.check_bays(&bay_states);
        // An active override's bay alert takes precedence over whatever
        // health-derived state that bay just got set to.
        self.reapply_bay_alert();
        self.recompute_status_led();
    }

    /// Re-asserts the currently active override's bay flash, if any. Needed
    /// after `update_health_leds` runs (which would otherwise clobber it
    /// with the plain health-derived state for that bay).
    fn reapply_bay_alert(&self) {
        if let Some(bay) = self.over.as_ref().filter(|o| o.level.always_visible()).and_then(|o| o.bay) {
            led::set_bay(bay, led::BayState::Alert);
        }
    }

    /// Called whenever the active override changes: starts/stops that
    /// bay's alert flash (a cheap, single-bay write -- not a full SMART
    /// re-scan) and recomputes the status LED to match.
    fn sync_leds_to_override(&mut self) {
        let want = self.over.as_ref().filter(|o| o.level.always_visible()).and_then(|o| o.bay);
        if self.alert_bay != want {
            if let Some(old) = self.alert_bay {
                led::set_bay(old, led::BayState::Normal);
            }
            self.alert_bay = want;
        }
        self.reapply_bay_alert();
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
    fn recompute_status_led(&self) {
        if let Some(ov) = &self.over {
            if let Some(pattern) = led::pattern_for_level(ov.level) {
                led::set_status(pattern);
                return;
            }
        }
        led::set_status(self.health_summary().pattern);
    }

    /// The same aggregate `recompute_status_led` turns into an LED
    /// pattern, plus the individual contributing pieces -- shared with
    /// `report::build` (the `status` socket request) so both use exactly
    /// this computation, not two copies that could drift. Deliberately
    /// does *not* factor in an active override (`self.over`) the way
    /// `recompute_status_led` does -- an override is what's currently
    /// being *shown*, this is the health computation underneath it,
    /// which the report labels separately (see `override_summary`).
    pub(crate) fn health_summary(&self) -> HealthSummary {
        use led::StatusPattern;

        let pool_healths = hal::pool_healths();
        let pool_degraded = pool_healths.iter().any(|(_, h)| h == "DEGRADED");
        let pool_faulted =
            pool_healths.iter().any(|(_, h)| matches!(h.as_str(), "FAULTED" | "UNAVAIL" | "OFFLINE"));
        let bay_failed = self.monitor.any_bay_failed();
        let temp_level = self.monitor.worst_temp_level();
        let network_level = self.network_health_level();

        // Worst of: this fan's health (pushed in from main.rs each tick,
        // since FanControllers live outside AppState), every currently-
        // connected temp sensor, monitored NIC link state, and whether any
        // bay is a confirmed SMART failure (Error -- a failed drive is
        // serious, same tier as a solid-red pool fault, even though it
        // doesn't necessarily mean the pool itself has degraded yet).
        let general = [self.fan_health, temp_level, network_level, if bay_failed { Level::Error } else { Level::Info }]
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
    pub(crate) fn override_summary(&self) -> Option<String> {
        self.over.as_ref().map(|o| {
            let bay = o.bay.map(|b| format!(" bay={b}")).unwrap_or_default();
            format!("{:?}{bay}: {} / {}", o.level, o.line0, o.line1)
        })
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
        let monitored: Vec<String> =
            if net.monitored_nics.is_empty() { hal::configured_nics() } else { net.monitored_nics.clone() };
        if monitored.is_empty() {
            return Level::Info; // nothing configured/in-service to check
        }
        let down = monitored.iter().filter(|i| led::link_is_down(i)).count();
        if down == 0 {
            Level::Info
        } else if down == monitored.len() {
            net.all_down_level.into()
        } else {
            net.some_down_level.into()
        }
    }

    pub fn set_eject_available(&mut self, available: bool) {
        self.eject_available = available;
    }

    /// Tells the state machine what the sleep schedule currently wants.
    /// Called every tick from `main`; actual sleep/wake transitions happen
    /// inside `tick()`, which also accounts for a recent manual wake and
    /// any active critical override before honoring it.
    pub fn set_schedule_sleep_wanted(&mut self, wanted: bool) {
        self.schedule_wants_sleep = wanted;
    }

    /// Rebuilds the flattened screen list from fresh HAL data. Called by
    /// the event loop right before a category is about to be (re)displayed,
    /// per the refresh-on-display design -- never on a background timer.
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
        let mut pools_or_hdd_changed = false;

        if self.cfg.screens.network && self.network_cache.stale(Duration::from_secs(r.network_min_secs)) {
            self.network_cache.screens = hal::network();
            self.network_cache.last_refresh = Some(Instant::now());
        }
        // Pool/hdd staleness (and so, health monitoring + LED updates) is
        // checked regardless of `cfg.screens.pools`/`.hdd` -- those only
        // gate the *display* screen, populated separately below. Alerting
        // has no business being silently disabled because someone turned
        // off an LCD screen.
        if self.pools_cache.stale(Duration::from_secs(r.pools_min_secs)) {
            self.monitor.check_pools(&hal::pool_healths());
            if self.cfg.screens.pools {
                self.pools_cache.screens = hal::pools();
            }
            self.pools_cache.last_refresh = Some(Instant::now());
            pools_or_hdd_changed = true;
        }
        if self.hdd_cache.stale(Duration::from_secs(r.hdd_min_secs)) {
            if self.cfg.screens.hdd {
                self.hdd_cache.screens = hal::hdd();
            }
            self.hdd_cache.last_refresh = Some(Instant::now());
            pools_or_hdd_changed = true;
        }
        if self.cfg.screens.temperature
            && self.temperature_cache.stale(Duration::from_secs(r.temperature_min_secs))
        {
            self.temperature_cache.screens = hal::cpu_and_fan(&self.cfg);
            self.temperature_cache.last_refresh = Some(Instant::now());
        }
        // Independent of the temperature screen/cache above -- see
        // HealthMonitor::maybe_check_temps.
        self.monitor.maybe_check_temps(&self.cfg);
        if self.cfg.screens.docker && self.docker_cache.stale(Duration::from_secs(r.docker_min_secs)) {
            self.docker_cache.screens = hal::docker_issues(&self.cfg.docker.ignore);
            self.docker_cache.last_refresh = Some(Instant::now());
        }

        let mut screens = Vec::new();
        screens.extend(self.network_cache.screens.clone());
        screens.extend(self.pools_cache.screens.clone());
        screens.extend(self.hdd_cache.screens.clone());
        screens.extend(self.temperature_cache.screens.clone());
        screens.extend(self.docker_cache.screens.clone());
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
        if pools_or_hdd_changed {
            self.update_health_leds();
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Effect {
        if self.sleeping {
            // Any key just wakes the panel; it is never also acted on.
            // Stays awake for one resume-after-inactivity period even
            // though the schedule still wants it asleep, so a groggy 2am
            // glance doesn't get plunged back into darkness mid-read.
            self.sleeping = false;
            self.awake_override_until =
                Some(Instant::now() + Duration::from_secs(self.cfg.rotation.resume_after_secs));
            self.refresh_all();
            return Effect::SetLcdPower(true);
        }

        match &mut self.mode {
            Mode::Status => match key {
                Key::Up => {
                    self.page(-1);
                    Effect::None
                }
                Key::Down => {
                    self.page(1);
                    Effect::None
                }
                Key::Enter => {
                    let mut options = vec![Action::Shutdown, Action::Restart];
                    if self.eject_available {
                        options.push(Action::Eject);
                    }
                    self.mode = Mode::ActionMenu { options, selection: 0 };
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
                        deadline: Instant::now() + Duration::from_secs(self.cfg.menu.confirm_timeout_secs),
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
        }
    }

    fn page(&mut self, delta: isize) {
        if self.screens.is_empty() {
            return;
        }
        let len = self.screens.len() as isize;
        let new = (self.index as isize + delta).rem_euclid(len);
        self.index = new as usize;
        self.auto_rotate = false;
        self.resume_at = Some(Instant::now() + Duration::from_secs(self.cfg.rotation.resume_after_secs));
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
            SocketCommand::Show { level, ttl_secs, bay, line0, line1 } => {
                // error/critical are meant to always be seen: force them to
                // persist regardless of whatever ttl the caller passed.
                let expires_at = if level.always_visible() || ttl_secs == 0 {
                    None
                } else {
                    Some(Instant::now() + Duration::from_secs(ttl_secs))
                };
                let ov = Override { level, expires_at, bay, line0, line1 };

                if matches!(self.mode, Mode::Status) {
                    // A higher (or equal) level can replace what's showing;
                    // a lower one never steps on a more severe active alert.
                    let blocked = self.over.as_ref().is_some_and(|o| level < o.level);
                    if !blocked {
                        if self.sleeping && !level.always_visible() {
                            // Non-urgent messages are dropped while asleep
                            // rather than waking the panel for something
                            // routine.
                        } else {
                            self.sleeping = false;
                            self.over = Some(ov);
                            self.reset_scroll();
                            self.sync_leds_to_override();
                        }
                    }
                } else {
                    // Never interrupt the action menu / confirm flow.
                    self.pending_over = Some(ov);
                }
            }
            // Handled directly in main.rs's loop (needs `fans`, which
            // lives outside AppState) before a command ever reaches here
            // -- never actually matched at runtime, just keeps this
            // exhaustive.
            SocketCommand::StatusRequest(_) => {}
        }
    }

    fn apply_pending_override(&mut self) {
        if let Some(ov) = self.pending_over.take() {
            self.over = Some(ov);
            self.reset_scroll();
            self.sync_leds_to_override();
        }
    }

    /// Called on every event-loop wakeup. Returns what to do and, for the
    /// caller's poll() timeout, `next_deadline()` should be consulted too.
    pub fn tick(&mut self) -> Effect {
        // Sleep/wake transitions take priority over everything else, but
        // never fire mid-menu-interaction, never re-sleep through an active
        // override (e.g. a critical alert still showing), and respect the
        // post-manual-wake grace period.
        let grace_active = self
            .awake_override_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false);
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
            led::enter_night_mode(&hal::physical_nics());
            return Effect::SetLcdPower(false);
        }
        if !self.schedule_wants_sleep && self.sleeping {
            self.sleeping = false;
            led::exit_night_mode(&hal::physical_nics());
            for iface in hal::physical_nics() {
                led::set_nic_mode(&iface, self.cfg.led.nic_mode);
            }
            self.refresh_all();
            return Effect::SetLcdPower(true);
        }
        if self.sleeping {
            return Effect::None;
        }

        self.refresh_stale();

        // Expire a timed-out override.
        if let Some(ov) = &self.over {
            if let Some(exp) = ov.expires_at {
                if Instant::now() >= exp {
                    self.over = None;
                    self.reset_scroll();
                    self.sync_leds_to_override();
                }
            }
        }

        // Resume auto-rotation after manual paging goes idle.
        if !self.auto_rotate {
            if let Some(resume_at) = self.resume_at {
                if Instant::now() >= resume_at {
                    self.auto_rotate = true;
                    self.resume_at = None;
                }
            }
        }

        // Auto-cancel a stale confirm screen.
        if let Mode::Confirm { deadline, .. } = &self.mode {
            if Instant::now() >= *deadline {
                self.mode = Mode::Status;
                self.apply_pending_override();
            }
        }

        // Advance rotation if it's this screen's turn to change and nothing
        // is currently scrolling (don't cut a scroll cycle short).
        if matches!(self.mode, Mode::Status)
            && self.over.is_none()
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
        if self.sleeping {
            return Effect::None;
        }

        let (raw0, raw1) = match &self.mode {
            Mode::Status => {
                if let Some(ov) = &self.over {
                    (ov.line0.clone(), ov.line1.clone())
                } else if let Some(s) = self.screens.get(self.index) {
                    (s.line0.clone(), s.line1.clone())
                } else {
                    (String::new(), String::new())
                }
            }
            Mode::ActionMenu { options, selection } => {
                let marker = |i: usize| if i == *selection { ">" } else { " " };
                let opt = |i: usize| options.get(i).map(|a| a.label()).unwrap_or("");
                (
                    format!("{}{}", marker(*selection), opt(*selection)),
                    "UP/DN ENTER BACK".to_string(),
                )
            }
            Mode::Confirm { action, .. } => {
                (format!("CONFIRM {}?", action.label()), "ENTER=yes BACK=no".to_string())
            }
        };

        let cap = self.cfg.display.scroll_max_chars;
        let line0 = self.scroll_step(0, truncate(&raw0, cap));
        let line1 = self.scroll_step(1, truncate(&raw1, cap));
        Effect::Render(line0, line1)
    }

    fn scroll_step(&mut self, which: u8, text: String) -> String {
        let cfg = &self.cfg.display;
        let scroll = if which == 0 { &mut self.scroll0 } else { &mut self.scroll1 };

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
            .map(|s| now.duration_since(s) < Duration::from_millis(cfg.scroll_pause_ms))
            .unwrap_or(false);

        if !in_start_pause {
            if let Some(last) = scroll.last_step {
                if now.duration_since(last) >= Duration::from_millis(cfg.scroll_step_ms) {
                    scroll.offset += 1;
                    scroll.last_step = Some(now);
                    // Loop with a gap of spaces between the end and restart.
                    let gap = 4;
                    if scroll.offset > chars.len() + gap {
                        scroll.offset = 0;
                        scroll.started = Some(now); // pause again at the loop point
                        scroll.completed_a_pass = true;
                    }
                }
            }
        }

        let mut window = String::with_capacity(16);
        let gap = 4;
        let padded_len = chars.len() + gap;
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
