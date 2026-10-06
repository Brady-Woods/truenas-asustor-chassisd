# Changelog

Versions follow `Cargo.toml`. Reconstructed from git history up to 1.3.0.

## Unreleased

### Added
- EuP / AC-loss check: reads the driver's
  `/sys/devices/platform/asustor/eup` and `ac_power_resume` (read-only;
  never written). Logs a WARNING at startup when EuP is on while
  Wake-on-LAN or a `power_on` schedule rule needs wake from soft-off, and
  shows both values (and that warning) in `lcm-status status` under
  "Platform power (BIOS)". Nothing changes with a driver that doesn't
  provide them.

## 1.4.0 -- 2026-10-02

### Added
- Chassis buzzer, ADM-style, behind a new `[buzzer]` section (off by
  default). One long beep (800 ms) when the daemon starts within five
  minutes of boot; one short beep (200 ms) before a shutdown or restart
  (including the OS stopping the service during a reboot), when a chassis
  `LOCATE` starts, and on warning/error/critical alerts (at most once a
  minute; only critical ones during the sleep window). Per-event keys:
  `boot`, `power`, `alerts`, `find_me`.
  - Drives the speaker bits of port 0x61 at ~2 kHz through `/dev/port`
    with Super I/O GP75 held high, as ADM's `asbuzzer_js.ko` does. The
    stock `pcspkr` tone is silent on the AS6704T, and so is the toggle
    with GP75 low.

### Changed
- `LOCATE` lights the status/bay LEDs solid amber just before blinking
  them. Does not fully remove the green-first look at the start of a
  chassis locate on the AS6704T; still open.

## 1.3.0 -- 2026-10-02
- `LOCATE`: blink a bay's (or the chassis') LEDs to find it.
- ADM-style weekly power schedule (RTC wake, shutdown, restart).
- Wake-on-LAN: keep `[wol] mode` applied on configured NICs.
- Fans: fixed mode with a critical-temperature override.
- NIC LEDs: load `ledtrig-netdev`, verify by read-back, log failures once.
- LCD display-off at night, front LED brightness, bay LED mode; daytime
  LEDs restored at startup.
- Fixed status LED lag and an overnight monitoring gap; LCD redraw; bays
  sorted.
- LCD protocol robustness: write pacing after the MCU speaks, resync on
  partial frames, reject too-early ACKs, redraw both lines after a lost
  frame; optional wire trace (`LCM_STATUS_TRACE=path`).
- Guard `SHOW` ttl overflow; build clean on musl and newer clippy.

## 1.2.0 -- 2026-10-02
- Configurable screen templates and rotation order.
- Fan control fails safe (full speed without sensors, handed back on
  exit); curve-commanded PWM 0 treated as a stop; manual mode asserted
  before kicks.
- External commands bounded by a timeout; fan control on its own thread.
- Socket clients handled on their own threads with I/O timeouts; unknown
  subcommands rejected; a second daemon refuses to start.
- Config validated beyond templates; unknown keys reported.
- Edition 2024 and clippy pedantic; assorted cleanups.
- Front-panel button frames no longer dropped mid-write; sleep-window
  timezone bug fixed; night mode blanks text instead of cutting
  `power:lcd`.

## 1.1.0 -- 2026-09-23
- `lcm-status status`: live terminal report from the running daemon.
- Syslog health monitoring (temps, fan speed, pool health, SMART); status
  LED reflects all monitored health.
- Multi-fan/multi-sensor fan config and the `fan-profile` calibration
  tool; per-sensor curve endpoints.

## 1.0.0 -- 2026-09-23
- Initial release: portable ASUSTOR LCM/LED driver for TrueNAS SCALE, fan
  control, deploy script.
