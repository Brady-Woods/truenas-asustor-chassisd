//! Scheduled power on / shutdown / restart: ADM's Control Panel ->
//! Hardware -> Power -> "Power Schedule", reimplemented for TrueNAS.
//!
//! **What ADM does** (from `emboardmand`'s `Power_Service_Handler` thread
//! and `hardware.js`/`libndal.so`'s config plumbing): a flat list of weekly
//! rules, each `{type, days, hour, minute}` with `type` one of
//! `power_on`/`power_off`/`restart`/`sleep` and days numbered 0 = Sunday.
//! Once a minute it compares the current day/hour/minute against every
//! rule: a matching `power_off`/`restart` shuts down/reboots right away
//! (skipped, and logged, while an mdadm array is reshaping), and the next
//! `power_on` is kept programmed into the RTC's wake alarm ("RTC time
//! settings is the same as previous, skipping" when nothing changed), so
//! the box wakes on schedule however it was turned off.
//!
//! **Here:** the same rule model (`[[power_schedule]]`, see
//! `config::PowerRule`), with three deliberate differences:
//!
//! - Shutdown/restart are *edge*-triggered on a scheduled minute boundary
//!   being crossed while the daemon is running (`Scheduler::poll`), never
//!   on "the current minute matches". A daemon that starts -- or restarts
//!   -- inside or after a scheduled minute does not fire it, so a restart
//!   rule can't reboot-loop on a fast boot and a crash-restart can't
//!   shut the box down twice. A clock step bigger than `MAX_GAP_SECS`
//!   (NTP correcting a bad RTC at boot) likewise fires nothing it skipped
//!   over.
//! - They show a front-panel countdown first (`AppState::start_countdown`)
//!   that any button cancels, instead of going down immediately.
//! - `sleep` (ADM's S3 rule type) isn't supported: TrueNAS doesn't suspend.
//!
//! **Waking up** uses `/sys/class/rtc/rtc0/wakealarm`, which takes UTC
//! epoch seconds whatever mode the RTC itself keeps time in, so the only
//! timezone work is finding the next matching *local* wall-clock minute --
//! done by stepping through real minute boundaries and asking libc's
//! `localtime_r` what each one is locally (`next_occurrence`), which gets
//! DST right for free. A wall-clock time that doesn't exist (spring
//! forward) is skipped that day; one that happens twice (fall back) counts
//! only the first time (`is_repeated_wall_time`).
//!
//! The wake alarm is only ever touched when at least one enabled
//! `power_on` rule exists; then the daemon owns it and re-asserts it on
//! startup, every `RTC_RECHECK_SECS`, as soon as the armed time passes,
//! after any clock jump, right before any shutdown/restart it runs itself,
//! and on exit.

use crate::config::PowerRule;
use crate::state::Action;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The RTC wake alarm sysfs file. Write `0` to disarm, or a future UTC
/// epoch time to arm; reads back empty while disarmed. The kernel refuses
/// (`EBUSY`) to replace an armed alarm with another, hence `0` first.
pub const WAKEALARM_PATH: &str = "/sys/class/rtc/rtc0/wakealarm";

/// Longest countdown accepted, so a typo can't hold a shutdown open for a
/// day (and the remaining time always fits the 16-char LCD line).
pub const MAX_COUNTDOWN_SECS: u64 = 3600;

/// A gap between two polls longer than this (or a backward step longer
/// than this) is a clock jump, not the event loop running late: nothing
/// in between is fired. Comfortably above the worst case of the loop
/// being stuck behind a few timed-out subprocesses (10s each, see
/// `hal::run`).
const MAX_GAP_SECS: i64 = 300;

/// How often the RTC wake alarm is re-read and re-armed if it isn't what
/// it should be (e.g. something else ran `rtcwake`).
const RTC_RECHECK_SECS: i64 = 60;

/// How far ahead `next_occurrence` looks: a week, plus a day of slack for
/// a DST change landing in the middle.
const LOOKAHEAD_MINUTES: i64 = 8 * 24 * 60;

/// How long before a scheduled `power_on` a shutdown can be at most and
/// still reliably be off in time for the alarm to wake it again: if the
/// alarm passes while the box is still running it is spent, and the NAS
/// stays off until the following one. Only used to warn at config load.
const SHUTDOWN_MARGIN_MINUTES: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    PowerOn,
    Shutdown,
    Restart,
}

impl PowerAction {
    /// The config spelling, plus ADM's own names for the same things.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "power_on" | "poweron" | "wake" => Some(PowerAction::PowerOn),
            "shutdown" | "power_off" | "poweroff" => Some(PowerAction::Shutdown),
            "restart" | "reboot" => Some(PowerAction::Restart),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PowerAction::PowerOn => "power on",
            PowerAction::Shutdown => "shutdown",
            PowerAction::Restart => "restart",
        }
    }

    /// The front-panel menu action that carries this out; `None` for
    /// power on, which is the RTC's job, not something run from here.
    fn menu_action(self) -> Option<Action> {
        match self {
            PowerAction::PowerOn => None,
            PowerAction::Shutdown => Some(Action::Shutdown),
            PowerAction::Restart => Some(Action::Restart),
        }
    }
}

/// One validated `[[power_schedule]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Bit `n` set = weekday `n`, 0 = Sunday (`tm_wday`'s and ADM's
    /// numbering).
    pub days: u8,
    /// Local wall-clock time, minutes since midnight.
    pub minute_of_day: u32,
    pub action: PowerAction,
    pub countdown_secs: u64,
    /// "#2 shutdown weekdays 23:00" -- for logs and diagnostics.
    pub label: String,
}

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const FULL_DAY_NAMES: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// `days` entries -> weekday bitmask. Accepts three-letter or full day
/// names, plus `daily`, `weekdays` (Mon-Fri) and `weekends` (Sat+Sun);
/// case-insensitive, mixable, duplicates harmless.
pub fn parse_days(days: &[String]) -> Result<u8, String> {
    if days.is_empty() {
        return Err("no `days`".into());
    }
    let mut mask = 0u8;
    for day in days {
        let d = day.trim().to_ascii_lowercase();
        mask |= match d.as_str() {
            "daily" | "everyday" | "all" => 0b111_1111,
            "weekdays" => 0b011_1110,
            "weekends" => 0b100_0001,
            _ => {
                let i = (0..7)
                    .find(|&i| d == DAY_NAMES[i] || d == FULL_DAY_NAMES[i])
                    .ok_or_else(|| {
                        format!("unknown day \"{day}\" (use sun..sat, daily, weekdays or weekends)")
                    })?;
                1 << i
            }
        };
    }
    Ok(mask)
}

/// A weekday bitmask as the shortest readable form, for labels.
fn days_text(mask: u8) -> String {
    match mask {
        0b111_1111 => "daily".into(),
        0b011_1110 => "weekdays".into(),
        0b100_0001 => "weekends".into(),
        _ => (0..7)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| DAY_NAMES[i])
            .collect::<Vec<_>>()
            .join(","),
    }
}

impl Rule {
    /// Validates one config entry; `index` is its 0-based position in the
    /// file, used only in the label.
    pub fn parse(index: usize, raw: &PowerRule) -> Result<Rule, String> {
        let action = PowerAction::parse(&raw.action).ok_or_else(|| {
            if raw.action.is_empty() {
                "no `action`".to_string()
            } else {
                format!(
                    "unknown action \"{}\" (use power_on, shutdown or restart)",
                    raw.action
                )
            }
        })?;
        let days = parse_days(&raw.days)?;
        let minute_of_day = crate::config::parse_hhmm(&raw.time)
            .ok_or_else(|| format!("time = \"{}\" is not a 24h HH:MM time", raw.time))?;
        if raw.countdown_secs > MAX_COUNTDOWN_SECS {
            return Err(format!(
                "countdown_secs = {} is over the {MAX_COUNTDOWN_SECS}s maximum",
                raw.countdown_secs
            ));
        }
        Ok(Rule {
            days,
            minute_of_day,
            action,
            countdown_secs: raw.countdown_secs,
            label: format!(
                "#{} {} {} {:02}:{:02}",
                index + 1,
                action.name(),
                days_text(days),
                minute_of_day / 60,
                minute_of_day % 60
            ),
        })
    }

    fn matches(&self, t: &LocalTime) -> bool {
        self.days & (1 << t.weekday) != 0 && t.hour * 60 + t.minute == self.minute_of_day
    }

    /// Every minute of the week (0 = Sunday 00:00) this rule fires at.
    fn week_minutes(&self) -> impl Iterator<Item = u32> + '_ {
        (0..7u32)
            .filter(|d| self.days & (1 << d) != 0)
            .map(|d| d * 1440 + self.minute_of_day)
    }
}

/// Every enabled, valid rule in `raw` -- invalid ones were already
/// reported and disabled by `Config::validate`, so they're just skipped.
pub fn parse_rules(raw: &[PowerRule]) -> Vec<Rule> {
    raw.iter()
        .enumerate()
        .filter(|(_, r)| r.enabled)
        .filter_map(|(i, r)| Rule::parse(i, r).ok())
        .collect()
}

/// Combinations of otherwise-valid rules that won't do what was probably
/// meant. Reported only; every rule still applies.
pub fn lint(rules: &[Rule]) -> Vec<String> {
    const WEEK: u32 = 7 * 1440;
    let mut out = Vec::new();
    let offs = || {
        rules
            .iter()
            .filter(|r| matches!(r.action, PowerAction::Shutdown | PowerAction::Restart))
    };
    for (i, a) in offs().enumerate() {
        for b in offs().skip(i + 1) {
            if a.action != b.action && a.week_minutes().any(|m| b.week_minutes().any(|n| n == m)) {
                out.push(format!(
                    "[[power_schedule]] {} and {} fall on the same minute; the shutdown wins",
                    a.label, b.label
                ));
            }
        }
    }
    for off in offs().filter(|r| r.action == PowerAction::Shutdown) {
        let window = u32::try_from(off.countdown_secs.div_ceil(60) + SHUTDOWN_MARGIN_MINUTES)
            .unwrap_or(u32::MAX);
        for on in rules.iter().filter(|r| r.action == PowerAction::PowerOn) {
            let too_soon = off
                .week_minutes()
                .any(|m| on.week_minutes().any(|n| (n + WEEK - m) % WEEK < window));
            if too_soon {
                out.push(format!(
                    "[[power_schedule]] {} is less than {window} minutes after {} (including its \
                     countdown): if the NAS isn't fully off by then the wake alarm passes \
                     unused and it stays off",
                    on.label, off.label
                ));
            }
        }
    }
    out
}

/// A moment in local wall-clock terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    /// Seconds east of UTC in effect at this moment (DST included).
    pub utc_offset: i64,
}

impl LocalTime {
    /// `t` (UTC epoch seconds) in the system's local timezone, via libc's
    /// `localtime_r` -- the same source `[sleep]` uses (see
    /// `main::now_hhmm` for why not `SystemTime` arithmetic).
    pub fn from_epoch(t: i64) -> LocalTime {
        let t: libc::time_t = t;
        // SAFETY: `tm` is plain data for which all-zeroes is valid, and
        // both pointers passed to the reentrant `localtime_r` are to
        // locals that outlive the call.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&raw const t, &raw mut tm);
            tm
        };
        let field = |v: libc::c_int| u32::try_from(v).unwrap_or(0);
        LocalTime {
            year: tm.tm_year + 1900,
            month: field(tm.tm_mon) + 1,
            day: field(tm.tm_mday),
            weekday: field(tm.tm_wday),
            hour: field(tm.tm_hour),
            minute: field(tm.tm_min),
            utc_offset: tm.tm_gmtoff,
        }
    }

    /// The wall-clock reading alone, without the offset that tells two
    /// readings of the same time apart across a DST fall-back.
    fn wall(&self) -> (i32, u32, u32, u32, u32) {
        (self.year, self.month, self.day, self.hour, self.minute)
    }
}

/// Current time as UTC epoch seconds.
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// True if the local wall-clock minute at `m` already happened once,
/// earlier, under a larger UTC offset -- i.e. `m` is in the hour a DST
/// fall-back repeats. Those count only the first time round, so a 01:30
/// restart doesn't reboot twice that night.
fn is_repeated_wall_time(m: i64, local: &impl Fn(i64) -> LocalTime) -> bool {
    let at = local(m);
    // DST shifts are at most a couple of hours; looking 4h back is sure to
    // see the offset from before any shift that `m` is still inside of.
    let shift = local(m - 4 * 3600).utc_offset - at.utc_offset;
    shift > 0 && local(m - shift).wall() == at.wall()
}

/// Every minute boundary in (`after`, `until`], as epoch seconds.
fn minute_boundaries(after: i64, until: i64) -> impl Iterator<Item = i64> {
    let first = after.div_euclid(60) * 60 + 60;
    (first..=until).step_by(60)
}

/// Rules (matching `filter`) due at the minute boundary `m`, if any.
fn due_at<'a>(
    rules: &'a [Rule],
    m: i64,
    local: &impl Fn(i64) -> LocalTime,
    filter: impl Fn(&Rule) -> bool,
) -> impl Iterator<Item = &'a Rule> {
    let t = local(m);
    let mut due = rules
        .iter()
        .filter(move |r| filter(r) && r.matches(&t))
        .peekable();
    // Only pay for the repeat check on a minute something actually matches.
    let repeated = due.peek().is_some() && is_repeated_wall_time(m, local);
    due.filter(move |_| !repeated)
}

/// The first minute boundary after `after` at which a rule matching
/// `filter` is due, and that rule. `None` if there's no such rule.
pub fn next_occurrence<'a>(
    rules: &'a [Rule],
    after: i64,
    local: &impl Fn(i64) -> LocalTime,
    filter: impl Fn(&Rule) -> bool + Copy,
) -> Option<(i64, &'a Rule)> {
    if !rules.iter().any(filter) {
        return None;
    }
    minute_boundaries(after, after + LOOKAHEAD_MINUTES * 60)
        .find_map(|m| best(due_at(rules, m, local, filter)).map(|r| (m, r)))
}

/// Of several rules due in the same minute, the one that runs: a shutdown
/// beats a restart (see `lint`).
fn best<'a>(rules: impl Iterator<Item = &'a Rule>) -> Option<&'a Rule> {
    rules.max_by_key(|r| r.action == PowerAction::Shutdown)
}

fn is_power_on(r: &Rule) -> bool {
    r.action == PowerAction::PowerOn
}

fn is_power_off(r: &Rule) -> bool {
    r.action != PowerAction::PowerOn
}

/// A scheduled shutdown/restart that just came due -- see `Scheduler::poll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub action: Action,
    pub countdown_secs: u64,
    pub label: String,
}

/// Runs the power schedule: fires shutdown/restart rules as their minute
/// arrives, and keeps the RTC wake alarm set for the next power on.
pub struct Scheduler {
    rules: Vec<Rule>,
    /// The wake alarm file, or `None` when there's no power-on rule --
    /// then the RTC is never touched, so an alarm set by anything else
    /// (e.g. a manual `rtcwake`) is left alone.
    rtc: Option<PathBuf>,
    /// High-water mark: every minute boundary at or before this has been
    /// dealt with. `None` until the first poll.
    checked_until: Option<i64>,
    /// What the RTC was last confirmed or set to; `None` if disarmed or
    /// unknown (after an error).
    armed: Option<i64>,
    next_rtc_check: i64,
    /// The last RTC error logged, so a persistent failure logs once
    /// rather than every `RTC_RECHECK_SECS`.
    rtc_error: Option<String>,
}

impl Scheduler {
    pub fn new(raw: &[PowerRule]) -> Self {
        Scheduler::with_rtc(parse_rules(raw), Path::new(WAKEALARM_PATH))
    }

    fn with_rtc(rules: Vec<Rule>, wakealarm: &Path) -> Self {
        let rtc = rules
            .iter()
            .any(is_power_on)
            .then(|| wakealarm.to_path_buf());
        Scheduler {
            rules,
            rtc,
            checked_until: None,
            armed: None,
            next_rtc_check: i64::MIN,
            rtc_error: None,
        }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Call every event-loop tick with the current time. Returns a
    /// shutdown/restart whose scheduled minute was crossed since the last
    /// call -- never one from before the first call (see the module doc),
    /// never one skipped over by a clock jump, and never the same one
    /// twice. Also keeps the wake alarm armed.
    pub fn poll(&mut self, now: i64) -> Option<Fired> {
        self.poll_with(now, &LocalTime::from_epoch)
    }

    fn poll_with(&mut self, now: i64, local: &impl Fn(i64) -> LocalTime) -> Option<Fired> {
        let mut fired = None;
        match self.checked_until {
            None => {
                self.checked_until = Some(now);
                self.next_rtc_check = i64::MIN;
            }
            Some(prev) if (now - prev).abs() > MAX_GAP_SECS => {
                crate::syslog::notice(&format!(
                    "power schedule: clock jumped by {}s; anything scheduled in between is skipped",
                    now - prev
                ));
                self.checked_until = Some(now);
                self.next_rtc_check = i64::MIN;
            }
            // A small step backwards keeps the high-water mark where it
            // was, so the minutes it re-lives don't fire a second time.
            Some(prev) if now <= prev => {}
            Some(prev) => {
                let due = minute_boundaries(prev, now)
                    .filter_map(|m| best(due_at(&self.rules, m, local, is_power_off)))
                    .last();
                fired = due.and_then(|r| {
                    Some(Fired {
                        action: r.action.menu_action()?,
                        countdown_secs: r.countdown_secs,
                        label: r.label.clone(),
                    })
                });
                self.checked_until = Some(now);
            }
        }
        if self.rtc.is_some()
            && (now >= self.next_rtc_check || self.armed.is_some_and(|a| now >= a))
        {
            self.sync_rtc_with(now, local);
        }
        fired
    }

    /// Makes sure the wake alarm is set for the next power on right now,
    /// rather than at the next periodic check -- called right before any
    /// shutdown/restart this daemon runs, and on exit.
    pub fn sync_rtc(&mut self, now: i64) {
        self.sync_rtc_with(now, &LocalTime::from_epoch);
    }

    fn sync_rtc_with(&mut self, now: i64, local: &impl Fn(i64) -> LocalTime) {
        let Some(path) = self.rtc.clone() else {
            return;
        };
        self.next_rtc_check = now + RTC_RECHECK_SECS;
        let want = next_occurrence(&self.rules, now, local, is_power_on);
        let want_at = want.map(|(t, _)| t);
        let result = (|| -> std::io::Result<bool> {
            if read_wakealarm(&path)? == want_at {
                return Ok(false);
            }
            std::fs::write(&path, "0")?;
            if let Some(t) = want_at {
                std::fs::write(&path, t.to_string())?;
            }
            Ok(true)
        })();
        match result {
            Ok(changed) => {
                if changed && let Some((t, rule)) = want {
                    crate::syslog::info(&format!(
                        "power schedule: RTC wake alarm set for {} ({})",
                        format_local(local(t)),
                        rule.label
                    ));
                }
                if self.rtc_error.take().is_some() {
                    crate::syslog::notice("power schedule: RTC wake alarm set successfully again");
                }
                self.armed = want_at;
            }
            Err(e) => {
                let hint = if e.kind() == std::io::ErrorKind::InvalidInput {
                    " (this RTC may not support alarms that far ahead)"
                } else {
                    ""
                };
                let msg = format!(
                    "power schedule: failed to set the RTC wake alarm at {}: {e}{hint}; \
                     scheduled power on will not happen",
                    path.display()
                );
                if self.rtc_error.as_deref() != Some(msg.as_str()) {
                    crate::syslog::warning(&msg);
                }
                self.rtc_error = Some(msg);
                self.armed = None;
            }
        }
    }

    /// Next scheduled events and the wake alarm's state, one line each,
    /// for the `STATUS` report and `check-config`. `read_rtc` adds what
    /// the RTC is actually set to (left off where there's no RTC to read,
    /// e.g. `check-config` on another machine).
    pub fn describe(&self, now: i64, read_rtc: bool) -> Vec<String> {
        describe_with(
            &self.rules,
            now,
            read_rtc,
            self.rtc.is_some(),
            &LocalTime::from_epoch,
        )
    }
}

fn describe_with(
    rules: &[Rule],
    now: i64,
    read_rtc: bool,
    manages_rtc: bool,
    local: &impl Fn(i64) -> LocalTime,
) -> Vec<String> {
    if rules.is_empty() {
        return vec!["no rules configured".into()];
    }
    let next = |filter: fn(&Rule) -> bool| -> String {
        next_occurrence(rules, now, local, filter).map_or_else(
            || "none".into(),
            |(t, r)| {
                format!(
                    "{} ({}, {})",
                    format_local(local(t)),
                    in_text(t - now),
                    r.label
                )
            },
        )
    };
    let mut out = vec![
        format!("next power on:   {}", next(is_power_on)),
        format!("next power off:  {}", next(is_power_off)),
    ];
    if read_rtc {
        let rtc = match read_wakealarm(Path::new(WAKEALARM_PATH)) {
            Ok(Some(t)) => format!("set for {}", format_local(local(t))),
            Ok(None) => "not set".into(),
            Err(e) => format!("unreadable ({e})"),
        };
        let owner = if manages_rtc {
            ""
        } else {
            " (not managed: no power_on rule)"
        };
        out.push(format!("RTC wake alarm:  {rtc}{owner}"));
    }
    out
}

/// The armed wake alarm, or `None` if disarmed (the file reads empty).
fn read_wakealarm(path: &Path) -> std::io::Result<Option<i64>> {
    let text = std::fs::read_to_string(path)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().map(Some).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected contents \"{text}\": {e}"),
        )
    })
}

/// "Mon 2026-10-05 07:30".
fn format_local(t: LocalTime) -> String {
    let day = DAY_NAMES[t.weekday as usize % 7];
    let day = format!("{}{}", day[..1].to_ascii_uppercase(), &day[1..]);
    format!(
        "{day} {:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// "in 2d 3h 4m" for a positive number of seconds.
fn in_text(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    let (d, h, m) = (mins / 1440, mins / 60 % 24, mins % 60);
    match (d, h) {
        (0, 0) => format!("in {m}m"),
        (0, _) => format!("in {h}h {m}m"),
        _ => format!("in {d}d {h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days since 1970-01-01 -> (year, month, day), Howard Hinnant's
    /// `civil_from_days` -- lets these tests run in any fixed timezone
    /// without touching the process's `TZ`.
    fn civil(days: i64) -> (i32, u32, u32) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap();
        let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap();
        let y = i32::try_from(yoe + era * 400 + i64::from(m <= 2)).unwrap();
        (y, m, d)
    }

    /// A timezone at `offset` seconds east of UTC.
    fn at_offset(t: i64, offset: i64) -> LocalTime {
        let l = t + offset;
        let days = l.div_euclid(86400);
        let secs = l.rem_euclid(86400);
        let (year, month, day) = civil(days);
        LocalTime {
            year,
            month,
            day,
            // 1970-01-01 was a Thursday.
            weekday: u32::try_from((days + 4).rem_euclid(7)).unwrap(),
            hour: u32::try_from(secs / 3600).unwrap(),
            minute: u32::try_from(secs / 60 % 60).unwrap(),
            utc_offset: offset,
        }
    }

    fn utc(t: i64) -> LocalTime {
        at_offset(t, 0)
    }

    /// US Pacific around its 2026 transitions: spring forward Sun Mar 8
    /// 02:00 PST (10:00 UTC), fall back Sun Nov 1 02:00 PDT (09:00 UTC).
    fn pacific(t: i64) -> LocalTime {
        const SPRING: i64 = 1_772_964_000; // 2026-03-08T10:00:00Z
        const FALL: i64 = 1_793_523_600; // 2026-11-01T09:00:00Z
        let dst = (SPRING..FALL).contains(&t);
        at_offset(t, if dst { -7 * 3600 } else { -8 * 3600 })
    }

    /// 2026-10-05 (a Monday) 00:00 UTC.
    const MON: i64 = 1_791_158_400;

    fn raw(days: &[&str], time: &str, action: &str) -> PowerRule {
        PowerRule {
            days: days.iter().map(|d| (*d).to_string()).collect(),
            time: time.into(),
            action: action.into(),
            ..PowerRule::default()
        }
    }

    fn rules(raw_rules: &[PowerRule]) -> Vec<Rule> {
        raw_rules
            .iter()
            .enumerate()
            .map(|(i, r)| Rule::parse(i, r).unwrap())
            .collect()
    }

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lcm-status-test-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn test_clock_is_right() {
        assert_eq!(utc(MON).weekday, 1);
        assert_eq!((utc(MON).year, utc(MON).month, utc(MON).day), (2026, 10, 5));
        let p = pacific(1_772_964_000);
        assert_eq!((p.month, p.day, p.hour, p.weekday), (3, 8, 3, 0));
    }

    #[test]
    fn day_shorthands_and_names() {
        let p = |d: &[&str]| parse_days(&d.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        assert_eq!(p(&["daily"]), Ok(0b111_1111));
        assert_eq!(p(&["weekdays"]), Ok(0b011_1110));
        assert_eq!(p(&["Weekends"]), Ok(0b100_0001));
        assert_eq!(p(&["sun", "Monday", "SAT"]), Ok(0b100_0011));
        assert_eq!(p(&["mon", "mon"]), Ok(0b10));
        assert!(p(&[]).is_err());
        assert!(p(&["mondays"]).is_err());
        assert!(p(&["funday"]).is_err());
    }

    #[test]
    fn bad_entries_are_rejected_with_a_reason() {
        assert!(
            Rule::parse(0, &raw(&["mon"], "24:00", "shutdown"))
                .unwrap_err()
                .contains("HH:MM")
        );
        assert!(
            Rule::parse(0, &raw(&["mon"], "07:00", "hibernate"))
                .unwrap_err()
                .contains("unknown action")
        );
        assert!(
            Rule::parse(0, &raw(&[], "07:00", "restart"))
                .unwrap_err()
                .contains("days")
        );
        let mut long = raw(&["mon"], "07:00", "shutdown");
        long.countdown_secs = MAX_COUNTDOWN_SECS + 1;
        assert!(Rule::parse(0, &long).is_err());
        // ADM's spelling works too.
        assert_eq!(
            Rule::parse(0, &raw(&["mon"], "07:00", "power_off"))
                .unwrap()
                .action,
            PowerAction::Shutdown
        );
    }

    #[test]
    fn next_occurrence_wraps_around_the_week() {
        // Sunday 08:00, asked on Monday: six days later.
        let r = rules(&[raw(&["sun"], "08:00", "power_on")]);
        let (t, _) = next_occurrence(&r, MON + 3600, &utc, is_power_on).unwrap();
        assert_eq!(t, MON + 6 * 86400 + 8 * 3600);
        // Exactly at the scheduled minute: that one's not "next" anymore.
        let r = rules(&[raw(&["mon"], "01:00", "power_on")]);
        let (t, _) = next_occurrence(&r, MON + 3600, &utc, is_power_on).unwrap();
        assert_eq!(t, MON + 7 * 86400 + 3600);
        assert!(next_occurrence(&r, MON, &utc, is_power_off).is_none());
    }

    #[test]
    fn next_occurrence_is_in_local_time() {
        // 07:30 Pacific (PDT, UTC-7) on Monday 2026-10-05 is 14:30 UTC.
        let r = rules(&[raw(&["weekdays"], "07:30", "power_on")]);
        let (t, _) = next_occurrence(&r, MON + 12 * 3600, &pacific, is_power_on).unwrap();
        assert_eq!(t, MON + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn nonexistent_spring_forward_time_is_skipped_that_day() {
        // 02:30 doesn't exist on Sun 2026-03-08 in Pacific time.
        let r = rules(&[raw(&["daily"], "02:30", "power_on")]);
        let sat_noon = 1_772_913_600; // 2026-03-07T20:00:00Z = 12:00 PST
        let (t, _) = next_occurrence(&r, sat_noon, &pacific, is_power_on).unwrap();
        let l = pacific(t);
        assert_eq!((l.month, l.day, l.hour, l.minute), (3, 9, 2, 30));
    }

    #[test]
    fn repeated_fall_back_time_fires_only_the_first_time() {
        // 01:30 happens twice on Sun 2026-11-01 in Pacific time: 08:30Z
        // (PDT) and 09:30Z (PST).
        let first = 1_793_521_800; // 2026-11-01T08:30:00Z
        let r = rules(&[raw(&["sun"], "01:30", "restart")]);
        let (t, _) = next_occurrence(&r, first - 3600, &pacific, is_power_off).unwrap();
        assert_eq!(t, first);
        let (t, _) = next_occurrence(&r, first, &pacific, is_power_off).unwrap();
        assert_eq!(t, first + 7 * 86400 + 3600, "the 09:30Z repeat is skipped");

        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        assert!(s.poll_with(first - 30, &pacific).is_none());
        assert!(s.poll_with(first + 1, &pacific).is_some());
        let mut repeats = 0;
        for t in (first + 2..first + 2 * 3600).step_by(20) {
            repeats += usize::from(s.poll_with(t, &pacific).is_some());
        }
        assert_eq!(repeats, 0);
    }

    #[test]
    fn fires_once_when_the_minute_is_crossed() {
        let r = rules(&[raw(&["mon"], "23:00", "shutdown")]);
        let due = MON + 23 * 3600;
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        assert!(s.poll_with(due - 5, &utc).is_none());
        assert!(s.poll_with(due - 1, &utc).is_none());
        let fired = s.poll_with(due, &utc).unwrap();
        assert_eq!(fired.action, Action::Shutdown);
        assert_eq!(fired.countdown_secs, 60);
        assert!(s.poll_with(due + 1, &utc).is_none());
        assert!(s.poll_with(due + 59, &utc).is_none());
        assert!(s.poll_with(due + 61, &utc).is_none());
    }

    #[test]
    fn a_late_poll_still_fires() {
        // The event loop was stuck behind a slow subprocess across the
        // boundary.
        let r = rules(&[raw(&["mon"], "23:00", "restart")]);
        let due = MON + 23 * 3600;
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        s.poll_with(due - 20, &utc);
        assert_eq!(s.poll_with(due + 25, &utc).unwrap().action, Action::Restart);
    }

    #[test]
    fn starting_inside_or_after_the_minute_never_fires_it() {
        // E.g. a restart rule at 03:00 and a box that reboots, or a daemon
        // that crash-restarts, within or shortly after that minute.
        let due = MON + 3 * 3600;
        for start in [due, due + 1, due + 59, due + 90, due + 600] {
            let mut s = Scheduler::with_rtc(
                rules(&[raw(&["daily"], "03:00", "restart")]),
                Path::new("/nonexistent"),
            );
            for t in start..start + 180 {
                assert!(s.poll_with(t, &utc).is_none(), "start {start}, t {t}");
            }
        }
    }

    #[test]
    fn a_clock_jump_fires_nothing_it_skipped_over() {
        let r = rules(&[raw(&["daily"], "03:00", "shutdown")]);
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        // NTP correcting a stale clock forward across 03:00.
        s.poll_with(MON + 3600, &utc);
        assert!(s.poll_with(MON + 4 * 3600, &utc).is_none());
        // ...but the schedule carries on normally afterwards. (Skipping
        // ahead a day in this test is itself a jump, so lands just short.)
        assert!(s.poll_with(MON + 86400 + 3 * 3600 - 1, &utc).is_none());
        assert!(s.poll_with(MON + 86400 + 3 * 3600 + 1, &utc).is_some());
    }

    #[test]
    fn a_small_backward_step_does_not_refire() {
        let r = rules(&[raw(&["mon"], "23:00", "shutdown")]);
        let due = MON + 23 * 3600;
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        s.poll_with(due - 1, &utc);
        assert!(s.poll_with(due + 1, &utc).is_some());
        s.poll_with(due - 30, &utc); // clock stepped back 31s
        for t in due - 29..due + 120 {
            assert!(s.poll_with(t, &utc).is_none());
        }
    }

    #[test]
    fn shutdown_beats_restart_in_the_same_minute() {
        let r = rules(&[
            raw(&["mon"], "23:00", "restart"),
            raw(&["daily"], "23:00", "shutdown"),
        ]);
        assert_eq!(lint(&r).len(), 1);
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        s.poll_with(MON + 23 * 3600 - 1, &utc);
        assert_eq!(
            s.poll_with(MON + 23 * 3600, &utc).unwrap().action,
            Action::Shutdown
        );
    }

    #[test]
    fn power_on_rules_never_fire_anything() {
        let r = rules(&[raw(&["daily"], "07:00", "power_on")]);
        let mut s = Scheduler::with_rtc(r, Path::new("/nonexistent"));
        for t in (MON..MON + 86400).step_by(30) {
            assert!(s.poll_with(t, &utc).is_none());
        }
    }

    #[test]
    fn rtc_is_armed_for_the_next_power_on_and_rearmed_after_it_passes() {
        let rtc = temp_file("wakealarm-arm", "");
        let r = rules(&[
            raw(&["mon"], "07:00", "power_on"),
            raw(&["tue"], "06:00", "power_on"),
            raw(&["mon"], "23:00", "shutdown"),
        ]);
        let mut s = Scheduler::with_rtc(r, &rtc);
        s.poll_with(MON + 3600, &utc);
        assert_eq!(read_wakealarm(&rtc).unwrap(), Some(MON + 7 * 3600));
        // The alarm goes off while the NAS is already running; the kernel
        // disarms it. As soon as its time has passed it's moved on.
        std::fs::write(&rtc, "").unwrap();
        s.poll_with(MON + 7 * 3600 + 1, &utc);
        assert_eq!(read_wakealarm(&rtc).unwrap(), Some(MON + 86400 + 6 * 3600));
        // Something else (`rtcwake`) changes it: put back within a minute.
        std::fs::write(&rtc, "123").unwrap();
        s.poll_with(MON + 7 * 3600 + 30, &utc);
        assert_eq!(read_wakealarm(&rtc).unwrap(), Some(123));
        s.poll_with(MON + 7 * 3600 + 62, &utc);
        assert_eq!(read_wakealarm(&rtc).unwrap(), Some(MON + 86400 + 6 * 3600));
        std::fs::remove_file(&rtc).unwrap();
    }

    #[test]
    fn rtc_is_left_alone_without_a_power_on_rule() {
        let rtc = temp_file("wakealarm-alone", "123");
        let mut s = Scheduler::with_rtc(rules(&[raw(&["mon"], "23:00", "shutdown")]), &rtc);
        s.poll_with(MON, &utc);
        s.sync_rtc_with(MON + 10, &utc);
        assert_eq!(read_wakealarm(&rtc).unwrap(), Some(123));
        std::fs::remove_file(&rtc).unwrap();
    }

    #[test]
    fn power_on_too_soon_after_a_shutdown_is_flagged() {
        // 23:58 Saturday shutdown + 60s countdown, 00:02 Sunday power on:
        // across both the day and the week boundary.
        let r = rules(&[
            raw(&["sat"], "23:58", "shutdown"),
            raw(&["sun"], "00:02", "power_on"),
        ]);
        assert_eq!(lint(&r).len(), 1, "{:?}", lint(&r));
        let r = rules(&[
            raw(&["sun"], "23:00", "shutdown"),
            raw(&["mon"], "00:02", "power_on"),
            raw(&["sat"], "23:00", "power_on"),
        ]);
        assert!(lint(&r).is_empty(), "{:?}", lint(&r));
    }

    #[test]
    fn describe_lists_the_next_events() {
        let r = rules(&[
            raw(&["weekdays"], "07:30", "power_on"),
            raw(&["daily"], "23:00", "shutdown"),
        ]);
        let lines = describe_with(&r, MON + 12 * 3600, false, true, &utc);
        assert_eq!(
            lines,
            [
                "next power on:   Tue 2026-10-06 07:30 (in 19h 30m, #1 power on weekdays 07:30)",
                "next power off:  Mon 2026-10-05 23:00 (in 11h 0m, #2 shutdown daily 23:00)",
            ]
        );
        assert_eq!(
            describe_with(&[], MON, true, false, &utc),
            ["no rules configured"]
        );
    }
}
