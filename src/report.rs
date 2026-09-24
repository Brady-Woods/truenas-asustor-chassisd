//! Assembles the human-readable report for the `STATUS` socket request
//! (`lcm-status status`, see `socket.rs`/`main.rs`). Ties together
//! `AppState::health_summary()` (the same computation that drives the
//! status LED), live `FanController` status lines, and fresh `hal::`
//! reads for temps/bays/network -- all in one place, so this and the LED
//! itself can never disagree about what's currently true.

use crate::config::Config;
use crate::fan::FanController;
use crate::state::AppState;
use crate::{hal, led};

pub fn build(state: &AppState, fans: &[FanController], cfg: &Config) -> String {
    let mut out = String::new();

    out.push_str("=== lcm-status report ===\n\n");

    out.push_str("-- Fans --\n");
    if fans.is_empty() {
        out.push_str("  (none configured)\n");
    }
    for f in fans {
        out.push_str(&format!("  {}\n", f.status_line()));
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
        out.push_str(&format!(
            "  {label:<40} {temp_c:>6.1}C  [{level:<8}] (warn {warn:.1}C / crit {crit:.1}C)\n"
        ));
    }
    out.push('\n');

    let summary = state.health_summary();

    out.push_str("-- Pools --\n");
    if summary.pool_healths.is_empty() {
        out.push_str("  (none found)\n");
    }
    for (name, health) in &summary.pool_healths {
        out.push_str(&format!("  {name:<16} {health}\n"));
    }
    out.push('\n');

    out.push_str("-- Drive bays (SMART) --\n");
    let bays = hal::bay_led_states();
    if bays.is_empty() {
        out.push_str("  (none found)\n");
    }
    for (bay, bay_state) in bays {
        out.push_str(&format!("  bay {bay}: {bay_state:?}\n"));
    }
    out.push('\n');

    out.push_str("-- Network --\n");
    let monitored: Vec<String> = if cfg.network.monitored_nics.is_empty() {
        hal::configured_nics()
    } else {
        cfg.network.monitored_nics.clone()
    };
    let with_ip = hal::ip_by_iface();
    for iface in hal::physical_nics() {
        let addr = with_ip.get(&iface).cloned().unwrap_or_else(|| hal::nic_link_text(&iface));
        let is_monitored = cfg.network.enabled && monitored.contains(&iface);
        let link = if led::link_is_down(&iface) { "down" } else { "up" };
        let tag = if is_monitored { "monitored" } else { "not monitored" };
        out.push_str(&format!("  {iface:<10} {addr:<20} {tag}, link {link}\n"));
    }
    out.push('\n');

    out.push_str("-- Active override --\n");
    out.push_str(&format!("  {}\n\n", state.override_summary().unwrap_or_else(|| "none".to_string())));

    out.push_str("-- Overall status LED --\n");
    out.push_str(&format!(
        "  pattern: {:?}\n  severity: {:?}  (fan={:?} temp={:?} network={:?} pool_degraded={} pool_faulted={} bay_failed={})\n",
        summary.pattern,
        summary.overall,
        summary.fan_health,
        summary.temp_level,
        summary.network_level,
        summary.pool_degraded,
        summary.pool_faulted,
        summary.bay_failed,
    ));

    out
}
