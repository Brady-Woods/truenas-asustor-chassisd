# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Versions follow `Cargo.toml` (package `lcm-status`). Entries up to 1.3.0
were reconstructed from git history.

## [Unreleased]

## [3.0.0] - 2026-10-06

Major because the socket protocol changed (see **BREAKING** below), the
minimum Rust is now 1.89, and the LCM serial port is opened exclusively.

### Added

- MIT license (`LICENSE`, also in the release tarball).
- **Fan failsafes.** A fan that is enabled but never taken over no longer
  fails silently (BIOS automatic mode stops the it8625 fan): if the pwm chip
  never resolves or its writes fail, a warning and fan health Warn follow
  after 30s and a critical after 180s; if the chip resolves but no sensor
  ever reads, the fan is taken over at `max_pwm` after 30s. A fan disabled by
  `Config::validate` is still left alone, but the diagnostic says BIOS mode
  may stop it and `STATUS` shows "disabled by config" with health Warn.
- **Partial sensor loss.** If one selector that used to read (a drive, the
  NIC) goes silent while others still read, the fan is held at `max_pwm`
  after a 10s grace (warning once, health Warn) and released only after that
  selector has read continuously for 30s; recovery logs a notice.
- **Fan thread hang detection.** The fan thread publishes a heartbeat; the
  main loop treats one older than 15s like a death (the daemon exits and
  systemd restarts it). Shutdown is bounded: it waits 5s for the fan thread,
  then forces manual mode at full speed on every enabled fan from a
  time-limited helper thread.
- **systemd watchdog.** A minimal `sd_notify` in safe std (`WATCHDOG=1`
  over `$NOTIFY_SOCKET`, path or abstract), sent by a dedicated thread only
  while the fan heartbeat is under 15s old and the main loop has made
  progress within 300s (so a slow `zpool`/`smartctl`/`docker` refresh does
  not get the daemon killed, but a wedged fan thread or main loop does); the
  unit gets `WatchdogSec=60` and `NotifyAccess=main`. A no-op without
  `NOTIFY_SOCKET`.
- **`lcm-status fan-failsafe [CONFIG]`** forces every enabled fan to manual
  mode at full speed (time-bounded, logged, no serial port or socket). It
  exits 0 when no fan chip exists and non-zero when a present fan could not
  be set. `lcm-status.service` runs it from `ExecStopPost`, so a daemon
  stopped by the watchdog, SIGKILL, the OOM killer or a crash no longer
  leaves the fan at its last manual PWM (on this board BIOS automatic mode
  stops the fan). `TimeoutStopSec=20`; `WatchdogSignal` stays at SIGABRT.
- **Service sandboxing.** `NoNewPrivileges`, `RestrictSUIDSGID`,
  `LockPersonality`, `PrivateTmp`, `ProtectHome`, `RestrictAddressFamilies`
  and a fixed `PATH`. `ProtectSystem`, `ProtectKernel*`, `PrivateDevices`
  and capability bounding are deliberately left out (the daemon writes
  sysfs, loads modules and talks to serial/input devices). Needs a NAS test.
- README "Security model" section: what socket group members can do and why
  the checkout must be root-owned and not shared.
- `deploy.sh` warns (never fails) when the checkout, `target/`, the binary
  or a parent directory is not root-owned or is group/world-writable, and
  when the checkout is under `/home` or `/root` (the unit's `ProtectHome=yes`
  would make it fail with 203/EXEC).
- CI: a `cargo-deny` advisories job (`deny.toml`), every third-party action
  pinned to a full commit SHA, and Dependabot for cargo and github-actions.

### Changed

- **The control socket's default group is now `builtin_administrators`**
  (gid 544, always present on TrueNAS SCALE) instead of a purpose-made
  `lcm-status` group, so administrators can use `lcm-status status` and
  `lcm-status locate` with no setup. `[socket] group` is still configurable.
  Existing installs need no action: a config that says
  `group = "lcm-status"` keeps working while that group exists, and the old
  group is never removed. A group missing from `/etc/group` still leaves
  the socket root-only, with a syslog warning. `deploy.sh` no longer
  creates a group (no more `midclt group.create`/`groupadd`); it only warns
  when the configured group is not found.
- The rear reset button is documented as a deliberate non-goal: TrueNAS has
  nothing to bind it to.
- Docs and comments no longer point at the private deployment repository.
- **BREAKING: socket protocol.** Requests the daemon used to accept loosely
  (an unknown level, extra or misspelled arguments such as a typo'd `bay=`
  on `SHOW`/`CLEAR`/`LOCATE`, `STATUS` with arguments, invalid UTF-8) are now
  answered with a one-line `ERR <reason>` and ignored; `Level::parse`
  rejects unknown levels instead of mapping them to Info. Valid
  fire-and-forget requests stay silent, so well-formed clients are
  unaffected. Check scripts that send sloppy requests.
- `lcm-status locate` and `status` now treat an `ERR ...` reply as an error:
  it is printed to stderr and the exit status is non-zero (`locate` used to
  print "sent" and exit 0 regardless).
- **MSRV is now Rust 1.89** (was 1.88; needed for `File::try_lock`).
- **The LCM serial port (`/dev/ttyS1`) is opened exclusively.** `settext`,
  `init` and `listen` can no longer interleave frames with the running
  daemon; while it runs they fail with an "in use" error. Stop the service
  to use them.
- `lcm-status listen <secs>` with a huge duration is a clear error (exit 2)
  rather than a panic.
- **Over-range temperatures are hot.** A reading above 125C used to be
  dropped as "not connected", so a sensor that failed high removed its fan's
  cooling. It is now kept and fan control pegs `max_pwm` for it regardless of
  thresholds. The health monitor alarms only when the configured thresholds
  are below 125C (the defaults are). Readings below -20C (this board's
  unwired it8625 inputs read -128C) and `_fault` inputs are still excluded;
  non-finite text ("nan", "inf") is never parsed into a temperature or RPM.
  A garbage-high NVMe slot would peg the fan; narrow the selector with
  `input = "temp1"` and check `lcm-status status` on first deploy.
- **hwmon chip matching is exact first, then prefix.** An exact `name` match
  wins and a prefix is only a fallback when nothing matches exactly; results
  are sorted numerically (`hwmon2` before `hwmon10`) and an empty name matches
  nothing. Existing names (`it8625`, `coretemp`, `drivetemp`, `nvme`)
  resolve as before.
- Sensors are read by a thread per selector, off the fan control path: one
  wedged `drivetemp` read can no longer freeze every fan's curve. A sample
  older than three intervals (at least 10s) counts as no reading; the
  resample interval is clamped to 1..=300s.
- **Bounded queues and connections.** Socket commands go through a bounded
  queue (64; a full queue answers `ERR busy`) and the main loop takes at most
  16 per pass, so shutdown and fan/LCD work always run. At most 16
  connections at once (extra ones get `ERR busy: too many connections`),
  each request must arrive within 5s overall, and the reply write has its
  own deadline. The beep queue holds 8 (extra beeps are dropped), chassis
  `LOCATE` beeps are limited to one per 30s, and `flush()` gives up after 5s.
- The accept loop spawns with `thread::Builder` and logs and drops the
  connection on failure; accept errors (EMFILE, ...) are logged once per run
  and backed off from instead of spinning.
- The socket is bound in a private 0700 staging directory, given mode 0660
  and its group there, and renamed into place; `chgrp(1)` is replaced by
  `std::os::unix::fs::chown` with the gid read from `/etc/group`, and a
  failure is a syslog warning. The socket file is removed on clean shutdown.
- `run_with_timeout` no longer spawns reader threads: stdout is drained by
  polling from the calling thread, output over 1 MiB abandons the command,
  and a killed child gets a short bounded wait before being parked for later
  reaping. It fails open: with 16 or more stuck (unkillable) children,
  commands still run, with a rate-limited warning.
- The serial write waits out `WouldBlock` (polling `POLLOUT` up to a
  deadline, continuing partial writes) instead of failing or leaving half a
  frame on the wire.
- Release builds enable `overflow-checks`; the power scheduler's clock
  deltas, minute boundaries, RTC recheck and countdown text use
  saturating/`abs_diff` arithmetic.

### Fixed

- Commands the daemon spawns (`zpool`, `smartctl`, `ip`, `docker`,
  `udevadm`, `modprobe`, `systemctl`) no longer inherit `NOTIFY_SOCKET` and
  the watchdog variables, which made systemd log a "notification message
  from PID ..., but reception only permitted for main PID" line for every
  one of them.
- `fan-failsafe` log lines carry the `lcm-status` program name.
- Builds cleanly under Rust 1.99's clippy (`assert_is_empty`, the
  deprecated `AtomicUsize::fetch_update`).
- The fan is taken over within about 500ms of start instead of one
  `update_secs` later (the first control pass waits briefly for the first
  sensor samples), and `[[fans]] update_secs` is clamped to 1..=60 with a
  diagnostic. Sensor reader threads are named by index, so a NUL byte in a
  `[[fans.sensors]]` chip name can no longer panic the fan thread.
- A config value such as `dwell_secs = 9223372036854775807` panicked the
  daemon on its first tick (`Instant + Duration` overflow). Every user
  duration feeding an `Instant` (rotation, menu, refresh) is clamped to a
  day, `scroll_max_chars`/`scroll_gap` to sane sizes, each with a diagnostic,
  and all deadlines in `state.rs` use `checked_add` with a far-future
  fallback.
- NaN/inf temperature thresholds and fan curve values were accepted (and
  never alerted); they now revert to defaults or are dropped, with a
  diagnostic.
- `localtime_r` returning NULL silently read as midnight (matters for
  `[sleep]`); it is checked, falls back to UTC with a one-time warning, and
  `now_hhmm` shares the same code.
- `LCM_STATUS_TRACE` is opened with `O_NOFOLLOW` and refused unless it is a
  regular file (the daemon runs as root); a refusal is logged.
- A repeating `LOCATE` could build minutes of beep backlog and block shutdown
  (see the bounded queues above).
- `deploy.sh` now escapes the unit's `ExecStart` path for `sed` and the
  `midclt` JSON arguments, so paths or group names containing `|`, `&`,
  backslash or a quote no longer corrupt the unit or the request.

### Security

- Socket group members can no longer flood the daemon (bounded queue,
  connection cap, per-request deadlines) or leave it spinning on accept
  errors; the socket is never created with the umask's mode.
- Exclusive serial port, `O_NOFOLLOW` trace file, systemd sandboxing and the
  `deploy.sh` ownership/permission warnings (above) reduce what a
  misbehaving local client or a writable checkout can do to a root daemon.
- CI audits dependencies (`cargo-deny`) and pins actions by SHA.

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

[Unreleased]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v3.0.0...HEAD
[3.0.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v2.1.0...v3.0.0
[2.1.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.4.0...v2.0.0
[1.4.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/Brady-Woods/truenas-asustor-chassisd/releases/tag/v1.0.0
