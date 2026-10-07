# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Versions follow `Cargo.toml` (package `lcm-status`). Entries up to 1.3.0
were reconstructed from git history.

## [Unreleased]

### Added

- MIT license (`LICENSE`, also in the release tarball).

### Changed

- Docs and comments no longer point at the private deployment repository.

## [2.1.0] - 2026-10-06

### Added
- AQC113 10GbE temperature (hwmon `enp9s0`, PHY/MAC): warn 80C / critical
  100C (ADM's LAN-chip curve) and a default fan sensor ramping 70C to full
  speed at 100C.
- A critical temperature on any of a fan's sensors pegs it at `max_pwm`
  until all are below warning, in curve mode as well as fixed.
- Each GitHub release, from this one on, carries a prebuilt static
  `x86_64-unknown-linux-musl` binary (tarball and SHA-256), built by a
  release workflow. `deploy.sh` still builds from source.
- `CONTRIBUTING.md`, issue forms and a pull request template; CI runs
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` and `sh -n deploy.sh` on every push and pull request. This
  changelog now follows Keep a Changelog 1.1.0.

### Changed
- The most critical active alarm (health checks and pushed error/critical
  messages) now decides both the status LED and the LCD, whose rotating
  screens give way to the alarm's message while it lasts. UP/DOWN peeks at
  the screens, except under a critical alarm. A pushed error no longer
  hides a worse health alarm's LED. `status` lists the active alarms.
- A temperature sensor that disappears is dropped from monitoring instead
  of keeping its last level forever.

### Fixed
- LED settings (bay LED mode, front LED brightness, the status and night
  LED state) are applied when the platform driver shows up after the
  daemon started -- the normal order at boot, where the driver is loaded
  by its own Post Init script -- and again after the driver is reloaded.
  Previously they stayed at the firmware defaults until a restart.
- `deploy.sh` as the boot-time Post Init script: skips the build when the
  binary was already built from the current source (so it no longer fails
  every boot because Docker isn't up yet), waits for Docker when a build is
  needed, and keeps the existing binary with a warning if Docker never comes
  up. Its POSTINIT timeout is raised to 600 s for a boot-time rebuild.
- The `lcm-status` group is created through the TrueNAS middleware, so it
  survives reboots (TrueNAS regenerates `/etc/group` at boot, which made the
  daemon's socket `chgrp` fail with "invalid group").

## [2.0.0] - 2026-10-06

Breaking: needs the asustor-platform-driver fork v0.3 or later; the
`/dev/port` buzzer and the hwmon `pwm3` brightness are gone.

Requirements:
- The asustor-platform-driver fork
  (https://github.com/Brady-Woods/asustor-platform-driver), `main`, v0.3
  or later, which replaces the upstream `nas-deploy` build: LEDs, the
  "ASUSTOR Buzzer" device, `lcd_power`, `eup`/`ac_power_resume`.
- Its vendored `it87` (`it87.ko`, replacing the kernel's and the old
  `asustor_it87`) loaded with `led_pwm=3 led_pwm_invert=1` (plus
  `force_pwm=1` on the AS6704T for fan control), for front LED brightness.
  Fan control is unchanged: `pwm1` on the hwmon device named `it8625`.

Upgrading from 1.4.0:
- Deploy the platform driver first -- the fork's `main` (v0.3 or later)
  `asustor` modules and its `it87` with `led_pwm=3 led_pwm_invert=1` --
  then this version. The driver's `deploy.sh` also removes 1.4.0's stale
  `it87_gp75` export. This version's `deploy.sh` refuses to build without
  `/sys/devices/platform/asustor` (which older drivers only created on
  some boards), and the daemon can't beep or set brightness with an older
  driver or `it87` (it says why in the journal).

### Added
- EuP / AC-loss check: reads the driver's
  `/sys/devices/platform/asustor/eup` and `ac_power_resume` (read-only;
  never written). Logs a WARNING at startup when EuP is on while
  Wake-on-LAN or a `power_on` schedule rule needs wake from soft-off, and
  shows both values (and that warning) in `lcm-status status` under
  "Platform power (BIOS)". Nothing changes with a driver that doesn't
  provide them.

### Changed
- Buzzer: now driven only through the platform driver's own buzzer input
  device, "ASUSTOR Buzzer" (asustor-platform-driver fork, v0.3 or
  later). Each beep is an `EV_SND`/`SND_TONE` 2000 Hz event to it, a
  wait, and `SND_TONE` 0 (always sent); the driver opens the GP75 gate by
  itself while the tone plays. `pcspkr` isn't needed. Same patterns
  and `[buzzer]` keys as 1.4.0.
- Beeps only when `/sys/devices/platform/asustor/buzzer_gate` reads
  `active` and the "ASUSTOR Buzzer" device exists (found by name).
  Otherwise one WARNING names why (no gate: older driver; `disabled`:
  `buzzer=0`; `unavailable`: a stale `it87_gp75` GPIO export when
  `asustor.ko` loaded; gate active but no device: a fork build from before
  v0.3, update and reload the driver). Re-checked before every beep, and
  logged when the buzzer becomes ready. `lcm-status status` shows it under
  "Buzzer".
- Platform driver detection (startup warning and `deploy.sh`): by the
  `/sys/devices/platform/asustor` directory instead of the
  `/sys/class/leds/power:lcd` LED, which the fork's driver (v0.3 or
  later) no longer has (LCD power is now its `lcd_power` rail, switched on
  and held by the driver). The daemon still never switches LCD power;
  night mode keeps using the panel's display-off command. `lcm-status
  status` shows `lcd_power` under "Front panel".
- Front LED brightness (`[led] brightness` / `night_brightness`): now the
  `/sys/class/leds/front_panel::brightness` LED that the fork's `it87`
  creates with `led_pwm=3 led_pwm_invert=1`, instead of the inverted hwmon
  `pwm3` (which no longer exists then). Percent is written as
  `round(percent * max_brightness / 100)` (0 = off, not inverted; the
  driver keeps the output in manual mode); with only `night_brightness`
  set, the level read at startup is put back on wake. If the LED is
  missing, one WARNING says it needs that `it87` (logged again only if the
  reason changes; a NOTICE when it applies again), and nothing else is
  written -- there's no fallback to `pwm3`. `lcm-status status` shows the
  level under "Front panel"; `deploy.sh` warns if the LED is missing.
- `lcm-status fan-profile` no longer special-cases `pwm3` (it isn't a
  hwmon output with `led_pwm=3`).

### Removed
- 1.4.0's way of beeping: toggling port 0x61 through `/dev/port` with
  GP75 exported through `/sys/class/gpio` for the daemon's lifetime. That
  export kept the platform driver from claiming GP75 (so its gate could
  never work), and the busy-wait tied up a CPU for each beep. The daemon
  no longer touches `/dev/port` or `/sys/class/gpio` at all.

## [1.4.0] - 2026-10-02

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

## [1.3.0] - 2026-10-02

### Added
- `LOCATE`: blink a bay's (or the chassis') LEDs to find it.
- ADM-style weekly power schedule (RTC wake, shutdown, restart).
- Wake-on-LAN: keep `[wol] mode` applied on configured NICs.
- Fans: fixed mode with a critical-temperature override.
- LCD display-off at night, front LED brightness, bay LED mode.
- Optional LCD wire trace (`LCM_STATUS_TRACE=path`).

### Changed
- NIC LEDs: load `ledtrig-netdev`, verify by read-back, log failures once.

### Fixed
- Daytime LEDs restored at startup.
- Status LED lag and an overnight monitoring gap; LCD redraw; bays
  sorted.
- LCD protocol robustness: write pacing after the MCU speaks, resync on
  partial frames, reject too-early ACKs, redraw both lines after a lost
  frame.
- Guard `SHOW` ttl overflow; build clean on musl and newer clippy.

## [1.2.0] - 2026-10-02

### Added
- Configurable screen templates and rotation order.

### Changed
- Fan control fails safe (full speed without sensors, handed back on
  exit); curve-commanded PWM 0 treated as a stop; manual mode asserted
  before kicks.
- External commands bounded by a timeout; fan control on its own thread.
- Socket clients handled on their own threads with I/O timeouts; unknown
  subcommands rejected; a second daemon refuses to start.
- Config validated beyond templates; unknown keys reported.
- Edition 2024 and clippy pedantic; assorted cleanups.

### Fixed
- Front-panel button frames no longer dropped mid-write; sleep-window
  timezone bug fixed; night mode blanks text instead of cutting
  `power:lcd`.

## [1.1.0] - 2026-09-23

### Added
- `lcm-status status`: live terminal report from the running daemon.
- Syslog health monitoring (temps, fan speed, pool health, SMART); status
  LED reflects all monitored health.
- Multi-fan/multi-sensor fan config and the `fan-profile` calibration
  tool; per-sensor curve endpoints.

## [1.0.0] - 2026-09-23

### Added
- Initial release: portable ASUSTOR LCM/LED driver for TrueNAS SCALE, fan
  control, deploy script.

[Unreleased]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.4.0...v2.0.0
[1.4.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/releases/tag/v1.0.0
