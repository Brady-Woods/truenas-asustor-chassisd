# lcm-status

Part of [`truenas-asustor-chassisd`](.) -- a portable, from-scratch driver
and status daemon for the ASUSTOR front-panel LCM (LCD module), front
LEDs, and fan control, for running ASUSTOR NAS hardware (built against an
AS6704T v2 / LOCKERSTOR 4 Gen2+) under TrueNAS SCALE instead of ADM. ADM's
own `lcmd` binary won't run on TrueNAS at all -- it's linked against
ASUSTOR's proprietary `libgeneral.so`/`libnasman.so`/`libnhal.so`/
`libndal.so`, none of which exist outside ADM, and it needs glibc symbol
versions newer than what TrueNAS ships. This project talks to the same
hardware directly instead of trying to run ADM's binary.

(Fan control isn't really "front-panel" hardware, but lives in this same
daemon anyway -- see "Fan control" below for why.)

## Requirements

- **[mafredri/asustor-platform-driver](https://github.com/mafredri/asustor-platform-driver), branch `nas-deploy`** (main + [PR #46](https://github.com/mafredri/asustor-platform-driver/pull/46), [#47](https://github.com/mafredri/asustor-platform-driver/pull/47), [#48](https://github.com/mafredri/asustor-platform-driver/pull/48)).
  All of the LED functionality (bay LEDs, status LED, LCD power/sleep) goes
  through the `/sys/class/leds` interface this driver creates. Stock TrueNAS
  SCALE does not include it. `deploy.sh` checks for it (kernel module
  loaded + expected LED class devices present) before doing anything else,
  and refuses to proceed if it's missing.
- Docker (TrueNAS's own Apps/Docker subsystem) -- used only to build in a
  throwaway `rust:alpine` container. Nothing is installed on the host
  toolchain-wise.
- A NAS built around the same platform (Jasper Lake + ASM1164 SATA
  controller + IT8625E Super I/O), or close enough that the LED names and
  serial protocol match. Some pieces (bay count, ATA port numbering) were
  derived empirically against this specific board and may need adjusting
  for other models.

## Quick start

```sh
sudo ./deploy.sh
```

Idempotent -- re-running it is always safe, and is in fact how it survives
TrueNAS upgrades (see below). It checks the driver dependency, builds,
installs the binary/config/systemd unit, and registers itself as a
TrueNAS-native boot task.

## Architecture

```
                    ┌────────────────────────────────┐
   /dev/ttyS1  <--> │  protocol.rs  (serial framing)  │
   (LCM MCU)        └────────────────────────────────┘
                                    │
   /sys/class/leds  <--------------┤  led.rs (sysfs LED control)
   /sys/class/net                  │
   /sys/class/hwmon                │
   zpool/smartctl/docker   ----->  │  hal.rs (data gathering)
                                    │
                              state.rs (the state machine)
                                    │
                      main.rs (event loop, systemd service)
                                    │
   /run/lcm-status.sock  <---------┘  socket.rs (external control)
```

- **`protocol.rs`** -- the reverse-engineered LCM serial protocol (see
  below). No ADM code or libraries; built entirely from disassembling
  ADM's `lcmd`/`libndal.so` and verifying every byte against the real
  hardware over SSH.
- **`hal.rs`** -- gathers the data each screen shows: network interfaces,
  ZFS pool capacity/health, per-drive SMART status, CPU/fan, Docker
  container health. Pure, synchronous, best-effort snapshots; callers
  decide the refresh cadence.
- **`led.rs`** -- thin sysfs wrapper for `/sys/class/leds/*` plus the
  status-LED decision logic (which pattern to show for which health
  state).
- **`state.rs`** -- the actual state machine: status rotation, the
  shutdown/restart/eject action menu, socket overrides, sleep. All the
  interactions between these live in one place (e.g. "a critical alert can
  preempt rotation but never a confirm screen").
- **`socket.rs`** -- the Unix socket other processes (an LED/status
  daemon, cron jobs, ad-hoc scripts) use to push text and drive LEDs
  without needing to know the wire protocol.
- **`main.rs`** -- ties it together: one `poll()`-based event loop, no
  busy-waiting. Idle (nothing scrolling, no pending timers) costs one
  wakeup per 100ms to check for a scroll/rotation tick, which is
  negligible; sleep mode costs nothing beyond that.

## The LCM (LCD) protocol

Reverse-engineered from ADM 5.1.4.RL21's `lcmd` and `libndal.so` by
disassembly, cross-referenced against the real hardware live over SSH
until every byte matched. There is no vendor documentation for any of
this -- everything below was derived, not looked up.

**Serial:** `/dev/ttyS1`, 115200 8N1 (`CS8|CLOCAL|CREAD`, `VMIN=1`), opened
`O_RDWR|O_NOCTTY|O_NONBLOCK`, `tcflush(TCIFLUSH)` on open.

**Frame format:**

```
[opcode][N][subcmd][... N payload bytes ...][checksum]
```

- `opcode`: `0xF0` for a request (either direction), `0xF1` for the ACK
  reply.
- `N`: payload length. **16 characters is the real, confirmed maximum for
  a text line** -- empirically verified by testing every length from 15 to
  28: only exactly 16 characters (`N=18`, i.e. 2 header bytes + 16 text
  bytes) is reliably accepted. Sizes above that get an explicit NAK
  (status `0x04`), not silent truncation -- there is no MCU-side
  auto-scroll or larger buffer to lean on.
- `checksum`: 8-bit sum of bytes `[0 .. N+2]` (i.e. everything except the
  checksum byte itself), stored at byte `N+3`. Wire length is `N+4`.

**Commands actually used:**

| opcode | subcmd | payload | meaning |
|---|---|---|---|
| `0xF0` | `0x11` | `[0x01]` | power-on init step 1 |
| `0xF0` | `0x22` | `[0x00]` | power-on init step 2 (~15ms after step 1) |
| `0xF0` | `0x27` | `[line][flag][16 ASCII bytes, space-padded]` | set text on `line` (0 or 1) |
| `0xF0` | `0x80` | `[key_code]` | **unsolicited**, MCU -> host: a button was pressed |
| `0xF0` | `0x13` | `[major][minor][patch]` | **unsolicited**, MCU -> host: firmware version report |
| `0xF1` | *(echoed)* | `[0x00]` | ACK for anything the host sent |

Any unsolicited `0xF0` frame from the MCU must be ACKed with `0xF1
<subcmd> [0x00]` or the MCU will eventually resend it.

**Button key codes** (subcmd `0x80` payload byte), confirmed by physically
pressing each button while running `lcm-status listen`:

| code | button |
|---|---|
| 1 | UP |
| 2 | DOWN |
| 3 | BACK |
| 4 | ENTER |
| 5 | wake-from-idle (not a distinct physical button) |

**No hardware scroll/marquee.** Confirmed by disassembling `libndal.so`:
`Ndal_Lcm_Set_Config`'s marquee functions (`MarqueeMode`, `MarqueeText`,
`LCM_Marquee_Enable`) only write to `nas.conf`'s `[LCM]` section via
`Set_Sect_Integer_Value`/`Set_Sect_String_Value` -- there's no distinct
serial opcode for "start scrolling" anywhere in the protocol. ADM's own
`lcmd` must have implemented scrolling by repeatedly resending shifted
16-char windows on a timer, which is exactly what `state.rs`'s
`scroll_step` does here (see `[display]` in the config for the rate/limit).

## LCD power (sleep mode)

The LCD's power is **not** part of the serial protocol at all -- it's a
separate GPIO line. Disassembling `Hal_Lcm_Set_Power` in `libnhal.so`
showed it calls into either `It87_Set_Gpio` or `Set_System_Gpio` depending
on platform ID, i.e. a Super I/O chip GPIO pin, not a UART command.

On this hardware that GPIO is already exposed cleanly by the platform
driver as a standard LED-class device:

```sh
echo 0 > /sys/class/leds/power:lcd/brightness   # off
echo 1 > /sys/class/leds/power:lcd/brightness   # on
```

Toggling it **power-cycles the LCD** -- confirmed live (off, then back on,
both verified visually against the physical panel). That means waking from
sleep needs the power-on init sequence (`0xF0/0x11` then `0xF0/0x22`)
resent, not just a resumed text write; `main.rs`/`state.rs` handle this on
every sleep->wake transition.

Per `asustord`'s own `LED-MODES.md`, `power:lcd` should **only** be used
by this project -- other LED "night mode" logic explicitly avoids it for
the same reboot-on-toggle reason.

## Front LEDs

Everything in `led.rs` is a thin wrapper over the standard Linux LED class
sysfs interface (`brightness`, `trigger`, `delay_on`, `delay_off`) that
`asustor-platform-driver` exposes. No `disk_led_ready` module-option
control is implemented here on purpose: changing it means
`rmmod asustor && modprobe asustor disk_led_ready=N`, which reloads the
whole shared LED driver and reboots the LCD as a side effect -- a
meaningfully bigger action than a sysfs write, left as an operator-set
`/etc/modprobe.d` option rather than something this daemon touches live.

**Status LED patterns:**

| Pattern | green | red | when |
|---|---|---|---|
| `Ok` | solid | off | nothing wrong |
| `NetworkDown` | solid | solid (= amber) | a physical NIC has no carrier |
| `Degraded` | solid | 500ms/500ms blink | a ZFS pool is `DEGRADED` (factory pattern) |
| `Failed` | off | solid | a pool is `FAULTED`/`UNAVAIL`/`OFFLINE`, or `error`-level socket alert |
| `CriticalFlashing` | off | 1000ms/1000ms blink | `critical`-level socket alert |

`red:status` is **not** one of the IT8625E's hardware-blinkable LEDs (see
`asustord`'s `LED-MODES.md`) -- it's software-timer-driven, so there's no
fixed menu of blink rates to pick from; 500/500 (factory Degraded) and
1000/1000 (our own Critical) were chosen to be clearly distinguishable
from each other, tuned by eye against the real hardware (an earlier
125ms/125ms attempt was confirmed correctly applied at the driver level
but too fast to visually read as blinking at all).

**Bay LEDs** (`sataN:green/red:disk`, bay number from
`/sys/class/ata_port/ataN/port_no` -- **not** the `ataN` name itself,
which is a global counter across every SATA controller in probe order and
would silently renumber if a second controller were ever added; `port_no`
is the same stable per-controller source the driver's own bay LED
triggers use):

| State | What |
|---|---|
| `Normal` | green LED handed back to the driver's own `asustor-sataN` activity trigger; red off |
| `Failed` | confirmed SMART failure -- solid red (factory pattern) |
| `Alert` | an `error`/`critical` socket message named this bay (`bay=N`) but it isn't a confirmed SMART failure -- 1000ms/1000ms red flash, same rate as the status LED |
| `Standby` | drive is spun down -- green flashes slowly (250ms/9750ms) |

## Fan control

Drives one or more pwm outputs from temperature -- see `src/fan.rs` (the
control loop) and `src/fan_calibrate.rs` (the `lcm-status fan-profile`
discovery tool, below). This used to be lm-sensors' `fancontrol` package's
job; it's implemented here now because `fancontrol` turned out not to be
part of TrueNAS SCALE's base image after all (see "Deploying, and
surviving TrueNAS upgrades" below), and reimplementing the curve logic
means fan control depends on nothing but this daemon.

The curve algorithm itself is ported directly from upstream
`fancontrol(8)`'s `UpdateFanSpeeds` -- linear ramp between `min_temp_c`
and `max_temp_c`, with `min_start_pwm`/`min_stop_pwm` stall hysteresis;
see the doc comment at the top of `fan.rs` for the one non-obvious bit
(the ramp's intercept at `min_temp_c` is `min_stop_pwm`, not `min_pwm`).
Config is `[[fans]]` in `lcm-status.toml`, one block per physical fan --
deliberately shaped like what `fancontrol`/`pwmconfig` expose (a pwm
output, a curve, and the sensor(s) that drive it), so it generalizes to
boards with more than one real fan, not just this one's single `it8625`
`pwm1`.

Three ways this goes further than upstream fancontrol:

- **Multiple sensors per fan.** `sensors` under a `[[fans]]` block is a
  list, not one fixed sensor. The default profile uses CPU package temp,
  every SATA drive (`drivetemp`), and every NVMe controller, so a hot
  drive ramps the fan even with the CPU idle. Each selector can be
  resampled on its own schedule (`min_resample_secs`) -- CPU temp can
  change quickly so it's read every tick by default; drive/NVMe temps
  change slowly and default to every 30s, read straight from
  `/sys/class/hwmon` with no `smartctl` calls.
- **Each sensor can have its own curve endpoints.** A selector's own
  `min_temp_c`/`max_temp_c` (falling back to the fan's, if unset) -- not
  just its own reading fed through one shared curve. This matters because
  different components reach their own danger zone at very different
  temperatures: this board's curve is CPU-tuned (`max_temp_c` = 90C), but
  a drive's own critical threshold ([Health monitoring](#health-monitoring-syslog)'s
  `[[temperature.thresholds]]`) is 60C -- without its own override, a
  drive at 60C would only compute to a modest partial speed against a
  curve tuned for a completely different component, not the full-speed
  response its own danger zone warrants. Each sensor is evaluated against
  its own curve independently and the fan runs at whichever demands the
  *highest resulting PWM* -- not "take the hottest raw reading, then feed
  it through one curve". The default profile's drive/NVMe selectors are
  set to match their `[[temperature.thresholds]]` entries for exactly
  this reason (keep them in sync if you change one).
- **Disconnected sensors are actually detected, not assumed.** A hwmon
  chip can expose more temp inputs than a given board wires up -- this
  board's `it8625` has `temp1`-`temp3` with no diode connected to any of
  them, reading a constant, wildly-out-of-range value forever.
  `hal::read_temp_input` treats a set `tempN_fault` flag, or a reading
  outside a generous plausible range, as "not connected" and excludes it,
  rather than letting a phantom sensor drag every fan to full speed
  forever. A `chip = "..."` selector with no `input` set matches *every*
  temp input that chip has, relying on this filtering rather than needing
  you to already know which specific inputs are real.

### Discovering what's actually connected: `lcm-status fan-profile`

```sh
sudo lcm-status fan-profile [config-path] [--yes]
```

The `pwmconfig` equivalent for this project. Read-only for sensors
(reports every temp input found, and whether it looks connected); for pwm
outputs it's necessarily invasive -- it takes over every pwm output it
finds for the duration (stopping `lcm-status.service` first if it's
running, restarting it when done), ramps each one through its range, and
measures what happens:

1. **Proves causation, not just correlation**, before crediting a pwm
   output with controlling a fan: ramps to max, then to a low value, then
   back to max, and only counts it if some fan's RPM actually drops at the
   low point and recovers afterward. A naive "ramp to max, see what's
   nonzero" check isn't enough -- found the hard way on this exact board,
   whose `it8625` exposes `pwm1` through `pwm6` in sysfs but has only
   `pwm1` wired to an actual fan header. The other five "detect" a
   response under a naive check purely because the one real fan is still
   drifting toward steady-state from whichever pwm was tested *previously*
   -- they do nothing at all when actually tested causally.
2. For each pwm output that does control a real fan, ramps it down to find
   where it stalls (empirical `min_stop_pwm`) and back up to find where it
   restarts (empirical `min_start_pwm`). Some fans (this board's included)
   never technically reach 0 RPM at any commanded duty -- the tool detects
   and reports that case explicitly rather than reporting a misleading
   `min_start_pwm`, but a low/zero empirical stall point still isn't
   necessarily a *good* PWM to run at continuously (noise, stability,
   wear) the way a real 0-RPM stall/restart threshold would be. Treat the
   numbers as a starting point to sanity-check, not a final answer --
   pwmconfig has the same limitation.
3. Prints a `[[fans]]` block per fan found, and the full sensor inventory
   with connected/unconnected verdicts, for you to review and assemble
   into `lcm-status.toml` -- it doesn't write your config for you. Which
   sensors should drive which fan, and what `min_temp_c`/`max_temp_c` to
   use, are judgment calls a PWM sweep can't make.

Restores every pwm output's original enable-mode/value when done,
regardless of what it found. `--yes` skips the confirmation prompt (for
non-interactive use); otherwise it asks before touching any hardware.

## Health monitoring (syslog)

`monitor.rs` logs to syslog (`syslog.rs`, real `syslog(3)` calls with
actual `LOG_WARNING`/`LOG_CRIT`/etc. priorities -- not just `eprintln!`
text, which is all one priority to journald no matter what it says) on
three kinds of transition, each gated so it logs once when a condition is
entered and once when it clears, never every poll for a sustained
condition:

| What | Warning | Critical |
|---|---|---|
| Any currently-connected temp sensor (not just CPU -- every SATA/NVMe/NIC/etc. sensor that reads as connected, see "Disconnected sensors are actually detected, not assumed" under Fan control) | per-chip `[[temperature.thresholds]]` if one matches, else `[temperature].warn_threshold` (default 75C) | same, `critical_threshold` (default 85C) |
| A configured fan's RPM | below `[[fans]].min_expected_rpm`, if set (spinning, but slower than it should be) | stalled (0 RPM) and still unresponsive after a few restart attempts (`fan.rs`'s `UNRESPONSIVE_AFTER_STALLS`) -- the first restart attempt alone only logs a warning, since a single transient stall that self-heals next tick isn't a fault |
| ZFS pool health (`zpool list`) | `DEGRADED` | `FAULTED`/`UNAVAIL`/`OFFLINE`/anything else that isn't `ONLINE` |
| Drive SMART status | -- | a bay's SMART overall-health reports `FAILED` |
| Monitored NIC links (`[network]`) | some (not all) monitored NICs down | all monitored NICs down |

### Per-chip temperature thresholds

One global warn/critical pair is a blunt instrument -- a CPU, an HDD, and
an NVMe SSD have very different safe operating ranges. `[[temperature.thresholds]]`
overrides the global pair per hwmon chip name; this board's defaults are
sourced from each part's actual datasheet (2026-09-23), not guessed --
see `config.rs`'s `default_temp_thresholds()` doc comment for the specifics
and sources:

| Chip | Warn | Critical | Source |
|---|---|---|---|
| `coretemp` (Intel Celeron N5105) | 85C | 100C | Intel ARK: TjMax (throttle point) = 105C |
| `drivetemp` (WD Ultrastar DC HC550) | 50C | 60C | WD datasheet: operating range 5-60C |
| `nvme` (WD Black SN750) | 60C | 70C | WD datasheet: operating (composite) temp 0-70C |

The AQC113 NIC's board-level PHY/MAC sensors deliberately have no entry
of their own -- no public datasheet with a numeric junction/case limit
was found for that chip (Marvell's technical datasheets aren't publicly
indexed the way Intel's/WD's are), so fabricating a specific-looking
number would be worse than just falling back to the generic default.
Worth adding if Marvell's actual datasheet ever turns up.

### Status LED reflects all of the above, not just pool/network

The status LED (`state::recompute_status_led`) folds every check above
(fan health, temps, SMART) plus pool health and monitored-NIC link state
into one severity, via the same `Level` (Info/Warn/Error/Critical) the
socket protocol uses -- so it's a genuine "is anything wrong" indicator,
not just pool+network the way it started out. Found live that this
mattered: the LED sat solid amber with every individual health check
green, because its network check originally looked at *every* physical
NIC (`physical_nics()`), including an installed-but-never-configured
AQC113 card with no cable -- indistinguishable from a real outage.
`[network]`'s `monitored_nics` (empty = auto, whatever currently has an
IP -- `hal::configured_nics()`) is what fixed that; see that function's
doc comment for the full story.

Pool `DEGRADED` still gets its own factory-documented blink pattern
(green solid, red flashing) rather than being folded into the generic
amber, since that's a real ASUSTOR-recognized signal worth keeping
distinguishable. Everything else collapses to plain Warn=amber/
Error=solid red/Critical=flashing red -- which specific thing tripped it
is on the LCD screens and in syslog, not encoded in the LED color.

The network screen itself also shows every physical NIC now (`hal::network`),
not just ones with an address -- an interface with a cable but no IP shows
`"connected, no IP"`, one with no cable shows `"disconnected"`. Before
this, both looked identical (silently absent from the screen).

Temperature monitoring runs on its own clock (`temperature_min_secs`,
`HealthMonitor::maybe_check_temps`), deliberately independent of whether
the temperature *screen* is enabled -- alerting has no business being
silently disabled because someone turned off an LCD screen. Pool/SMART
monitoring piggybacks on the same `zpool`/`smartctl` calls the pools/hdd
screens and bay LEDs already make (on `pools_min_secs`/`hdd_min_secs`),
so this adds no new polling beyond what already existed -- again
independent of whether those screens are displayed, only of whether the
underlying data gets *fetched* (which now happens regardless).

Check what actually got logged:

```sh
journalctl -u lcm-status -p warning   # warnings and above
journalctl -u lcm-status -p crit      # critical only
```

## The socket protocol

`/run/lcm-status.sock` (configurable), a Unix socket owned `root:lcm-status`
(`0660`) so a non-root LED/status daemon can be added to that group instead
of needing to run as root. Newline-delimited; each connection sends one
message, optionally a header line plus up to two content lines, then
closes.

```
SHOW <level> <ttl_secs> [bay=N]
<line0>
<line1>

CLEAR [bay=N]
```

- `level`: `info` | `warn` | `error` | `critical`. Ordered -- a higher
  level can replace what's currently showing; a lower one never steps on
  a more severe active alert. `error` and `critical` **always** persist
  (ignore whatever `ttl_secs` was passed) and wake the panel from
  scheduled sleep; `info`/`warn` respect their TTL and are simply dropped
  (not queued) if they'd otherwise wake a sleeping panel.
- `ttl_secs`: auto-revert to normal rotation after this many seconds;
  `0` means persist until `CLEAR` or a higher-level message replaces it.
- `bay=N` (optional): also flashes that bay's red LED (`Alert` state,
  distinct from a confirmed SMART `Failed`) for as long as the message is
  showing. Only meaningful with `error`/`critical`.
- `line0`/`line1`: each truncated to `[display].scroll_max_chars`
  (default 64) and auto-scrolled if over 16 characters, at
  `[display].scroll_step_ms` per character-step.
- An active override **never** interrupts the button-driven action menu
  or a shutdown/restart/eject confirm screen -- it's queued and applied
  the instant the user backs out or confirms.

**Bare text shorthand** -- a message with no `SHOW`/`CLEAR` header is
`SHOW info 5` with the first line as `line0` and the second (if any) as
`line1`:

```sh
printf "Backup done\n42 files\n" | nc -U /run/lcm-status.sock -q1
```

**A critical drive alert, with the bay LED flashing too:**

```sh
printf "SHOW critical 0 bay=2\nDRIVE FAILURE\nCheck bay 2\n" \
  | nc -U /run/lcm-status.sock -q1
```

**Clear it:**

```sh
printf "CLEAR\n" | nc -U /run/lcm-status.sock -q1
```

### `STATUS`: dump a live health report to the terminal

```sh
sudo lcm-status status [config-path]
```

The one request/response exception to the fire-and-forget protocol above:
sends `STATUS`, and the running daemon writes back a full terminal-
readable report (`report.rs`) instead of just accepting a display
command. Needs root or membership in the socket's group (same
`root:lcm-status`, `0660` as everything else here) -- there's nothing
being written to the display, but the report itself covers privileged
reads (SMART, pool health).

Talks to whatever's *actually running*, not a fresh recomputation --
`AppState::health_summary()` (the exact same computation driving the
status LED, so this can never disagree with what the LED shows),
`FanController::status_line()` for each configured fan (live pwm/RPM plus
accumulated stall/low-RPM health only the running process knows, not
something a brand-new invocation could reconstruct), fresh `hal::` reads
for every connected temp sensor (with its resolved threshold and current
level), every pool, every drive bay's SMART state, and every physical
NIC's link/monitoring status, plus the active socket override if any:

```
=== lcm-status report ===

-- Fans --
  chassis (pwm1): pwm=104 (40%) 1339rpm [Info]

-- Temperatures --
  coretemp temp1 "Package id 0"              56.0C  [ok      ] (warn 85.0C / crit 100.0C)
  drivetemp temp1                            37.0C  [ok      ] (warn 50.0C / crit 60.0C)
  ...

-- Pools --
  HDD              ONLINE
  ...

-- Drive bays (SMART) --
  bay 1: Normal
  ...

-- Network --
  enp2s0     192.168.1.196        monitored, link up
  enp9s0     disconnected         not monitored, link down

-- Active override --
  none

-- Overall status LED --
  pattern: Ok
  severity: Info  (fan=Info temp=Info network=Info pool_degraded=false pool_faulted=false bay_failed=false)
```

Implementation note, if you're extending the protocol further: `handle_connection`
keeps a second cloned handle to the stream for writing the response after
the `BufReader` has consumed the original for reading -- a Unix stream
socket's two directions are independent, so this works even though a
plain read loop would otherwise "consume" the connection. The client
(`request_status` in `main.rs`) sends its request, then shuts down just
its *write* half (`Shutdown::Write`) so the daemon's `.lines()` sees EOF
and stops waiting for more input, while the read half stays open to
receive the reply.

## Config

`/etc/lcm-status.toml`, hand-edited, every field defaulted -- see
`lcm-status.example.toml` for the full annotated reference (scroll
speed/limits, per-category refresh floors, sleep schedule, which screens
are enabled, NIC LED mode, Docker containers to ignore, temperature
units/warning threshold).

## Deploying, and surviving TrueNAS upgrades

```sh
sudo ./deploy.sh
```

is the whole build+install pipeline: checks the driver dependency first
and refuses to continue without it, builds in a throwaway container
(nothing installed on the host toolchain-wise), installs the config only
if one doesn't already exist (never clobbers edits), installs and enables
the systemd unit, and registers itself as a TrueNAS **POSTINIT
Init/Shutdown Script** (`midclt call initshutdownscript.query`) so the
whole thing re-runs on every boot.

That last part is what makes this durable across TrueNAS updates: SCALE
updates land in a new ZFS boot environment (`boot-pool/ROOT/<version>`),
and `/usr` -- including `/usr/local` -- is **read-only by default** on a
fresh one. The binary is deliberately never installed there: it runs
straight out of this checkout (the systemd unit's `ExecStart` points at
`target/release/lcm-status` in place, substituted in by `deploy.sh`).
`/etc/systemd/system` and `/etc/lcm-status.toml` *are* writable, just not
carried forward from update to update -- which is fine, since `deploy.sh`
reinstalls both every run rather than treating first-install as special.
What's actually durable is TrueNAS's own config database, which is where
Init/Shutdown Scripts live. So even a fresh boot environment missing the
group, the systemd unit, and everything under `/etc` will self-heal on its
very first boot, with no manual re-deploy step. The source tree itself
lives under the data pool (`/mnt/.../home/...`, not the boot pool), for
the same reason -- it's storage TrueNAS updates never touch.

(This was found the hard way: an earlier version of this script installed
the binary to `/usr/local/sbin` and worked fine across same-kernel
reboots, because those all reused the one boot environment that had been
hand-patched `readonly=off` months before this project existed. The first
*real* TrueNAS update it went through landed on a genuinely fresh boot
environment and failed with `Read-only file system` -- see
`truenas-asustor-deploy`'s `docs/RUNBOOK.md` for the full story, which hit
the identical issue in the platform driver at the same time.)

## Manual testing / probing

The binary doubles as a CLI for testing against the hardware directly
(all of this is how the protocol above was originally verified):

```sh
lcm-status init                    # send the power-on sequence
lcm-status settext 0 "HELLO"       # write up to 16 chars to line 0 or 1
lcm-status listen 60               # print every unsolicited frame (button
                                    # presses, MCU version reports) for 60s
lcm-status hal-test                # dump everything hal.rs would gather
lcm-status check-config [path]     # parse and print a config file
```
