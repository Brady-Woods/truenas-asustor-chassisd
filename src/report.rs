//! Assembles the human-readable report for the `STATUS` socket request
//! (`lcm-status status`, see `socket.rs`/`main.rs`). Ties together
//! `AppState::health_summary()` (the same computation that drives the
//! status LED), live fan status lines, the power schedule's next events,
//! and `hal::` reads for temps/bays/network -- all in one place, so this
//! and the LED itself can never disagree about what's currently true.

// `write!` to a `String` can't fail, so its `fmt::Result` is ignored below.

use crate::config::Config;
use crate::fan::FanStatus;
use crate::hal;
use crate::power::{self, Scheduler};
use crate::state::AppState;
use std::fmt::Write;

pub fn build(state: &AppState, fans: &FanStatus, power: &Scheduler, cfg: &Config) -> String {
    let mut out = String::new();

    out.push_str("=== lcm-status report ===\n\n");

    out.push_str("-- Fans --\n");
    if fans.lines.is_empty() {
        out.push_str("  (none configured)\n");
    }
    for line in &fans.lines {
        let _ = writeln!(out, "  {line}");
    }
    out.push('\n');

    out.push_str("-- Temperatures --\n");
    let mut temps = hal::all_connected_temps();
    temps.sort_by(|a, b| a.1.cmp(&b.1));
    if temps.is_empty() {
        out.push_str("  (no connected sensors found)\n");
    }
    for (chip, label, temp_c) in temps {
        let (warn, crit) = crate::config::resolve_temp_threshold(&cfg.temperature, &chip);
        let level = if temp_c >= crit {
            "CRITICAL"
        } else if temp_c >= warn {
            "WARNING"
        } else {
            "ok"
        };
        let _ = writeln!(
            out,
            "  {label:<40} {temp_c:>6.1}C  [{level:<8}] (warn {warn:.1}C / crit {crit:.1}C)"
        );
    }
    out.push('\n');

    let summary = state.health_summary();

    out.push_str("-- Pools --\n");
    if summary.pool_healths.is_empty() {
        out.push_str("  (none found)\n");
    }
    for (name, health) in &summary.pool_healths {
        let _ = writeln!(out, "  {name:<16} {health}");
    }
    out.push('\n');

    out.push_str("-- Drive bays (SMART) --\n");
    let bays = state.bay_states();
    if bays.is_empty() {
        out.push_str("  (none found)\n");
    }
    for (bay, bay_state) in bays {
        let _ = writeln!(out, "  bay {bay}: {bay_state:?}");
    }
    out.push('\n');

    out.push_str("-- Network --\n");
    let monitored = hal::monitored_nics(&cfg.network);
    let with_ip = hal::ip_by_iface();
    for iface in hal::physical_nics() {
        let addr = with_ip
            .get(&iface)
            .cloned()
            .unwrap_or_else(|| hal::nic_link_text(&iface));
        let link = if hal::link_is_down(&iface) {
            "down"
        } else {
            "up"
        };
        let tag = if cfg.network.enabled && monitored.contains(&iface) {
            "monitored"
        } else {
            "not monitored"
        };
        let _ = writeln!(out, "  {iface:<10} {addr:<20} {tag}, link {link}");
    }
    out.push('\n');

    out.push_str("-- Power schedule --\n");
    if let Some(countdown) = state.countdown_summary() {
        let _ = writeln!(out, "  COUNTING DOWN: {countdown}");
    }
    for line in power.describe(power::now_epoch(), true) {
        let _ = writeln!(out, "  {line}");
    }
    out.push('\n');

    out.push_str("-- Active override --\n");
    let active = state.override_summary();
    let _ = writeln!(out, "  {}\n", active.as_deref().unwrap_or("none"));

    out.push_str("-- Overall status LED --\n");
    let _ = writeln!(out, "  pattern: {:?}", summary.pattern);
    let _ = writeln!(
        out,
        "  severity: {:?}  (fan={:?} temp={:?} network={:?} pool_degraded={} pool_faulted={} bay_failed={})",
        summary.overall,
        summary.fan_health,
        summary.temp_level,
        summary.network_level,
        summary.pool_degraded,
        summary.pool_faulted,
        summary.bay_failed,
    );

    out
}
