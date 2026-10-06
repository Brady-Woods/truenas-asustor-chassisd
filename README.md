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

- **The [asustor-platform-driver fork](https://github.com/Brady-Woods/asustor-platform-driver), `main`, v0.3 or later**
  (a fork of [mafredri/asustor-platform-driver](https://github.com/mafredri/asustor-platform-driver):
  upstream plus [PR #46](https://github.com/mafredri/asustor-platform-driver/pull/46),
  [#47](https://github.com/mafredri/asustor-platform-driver/pull/47),
  [#48](https://github.com/mafredri/asustor-platform-driver/pull/48) and the
  ADM-parity changes: buzzer device, LCD power rail, power settings, reset
  button). All of the LED functionality (bay LEDs, status LED) goes through
  the `/sys/class/leds` interface this driver creates, and the buzzer
  through its "ASUSTOR Buzzer" input device. Stock TrueNAS SCALE does not
  include it. `deploy.sh` checks for it (kernel module loaded,
  `/sys/devices/platform/asustor` and the expected LED class devices
  present) before doing anything else, and refuses to proceed if it's
  missing.
- **The fork's vendored `it87`** (built as `it87.ko`, replacing the
  kernel's), loaded with `force_pwm=1 led_pwm=3 led_pwm_invert=1`
  (e.g. `options it87 force_pwm=1 led_pwm=3 led_pwm_invert=1` in
  `/etc/modprobe.d/it87.conf`): fan control (`pwm1` on the `it8625` hwmon
  device, found by its hwmon name) and front LED brightness
  (`front_panel::brightness`). Only brightness needs `led_pwm`; without it
  everything else works.
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
  shutdown/restart action menu, the scheduled-shutdown countdown, socket
  overrides, locate, sleep. All the
  interactions between these live in one place (e.g. "a critical alert can
  preempt rotation but never a confirm screen").
- **`power.rs`** -- the weekly power schedule (`[[power_schedule]]`):
  fires scheduled shutdowns/restarts into `state.rs`'s countdown screen,
  and keeps the RTC wake alarm set for the next scheduled power on. See
  "Power schedule" below.
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
| `0xF0` | `0x11` | `[on]` | display on (1) / off (0), backlight included; with 1 it's also power-on init step 1 -- see "LCD power (sleep mode)" |
| `0xF0` | `0x22` | `[0x00]` | power-on init step 2 (~15ms after step 1) |
| `0xF0` | `0x27` | `[line][flag][16 ASCII bytes, space-padded]` | set text on `line` (0 or 1) |
| `0xF0` | `0x80` | `[key_code]` | **unsolicited**, MCU -> host: a button was pressed |
| `0xF0` | `0x13` | `[major][minor][patch]` | **unsolicited**, MCU -> host: firmware version report |
| `0xF1` | *(echoed)* | `[0x00]` | ACK for anything the host sent |

Any unsolicited `0xF0` frame from the MCU must be ACKed with `0xF1
<subcmd> [0x00]` or the MCU will eventually resend it.

**Pacing and lost frames:** wait ~20ms after the MCU sends anything
before sending the next frame -- a frame sent right after its ACK for
the previous one went unanswered ~1 time in 7 (found with a byte-level
wire trace, 2026-10-02). Even paced, the MCU still drops part of a frame
now and then on its own (a few percent of writes in testing, at any frame
rate and with any text; ADM's `lcmd` retries too, and restarts itself
after 100 straight failures). It then completes the partial frame with
the start of the next one and ACKs *that* -- and taking that ACK for the
next frame's own is what left stray text on the panel (the tail of one
line's IP address showing on the other line), since the daemon then
believed a line was showing that never was. So the daemon:

- waits `protocol::SETTLE` (20ms) after anything from the MCU before
  writing;
- ignores an ACK that arrives before the frame could even have finished
  arriving at 115200 baud (`protocol::wire_time`);
- retries an unACKed write after 100ms, and treats the *other* line as
  unknown too, so the next render rewrites both;
- resyncs its own receive side on the next `0xF0`/`0xF1` byte;
- resends both lines every 60s regardless, and redraws everything if the
  MCU reports a (re)boot.

`lcm-status status` reports text writes/retries/failures since startup
under `-- LCD link --`. Retries are expected now and then; failures
(all three attempts lost) should be rare. Set `LCM_STATUS_TRACE=/path`
in the daemon's environment to log every byte to and from the panel.

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
separate GPIO line, a power rail that the platform driver switches on when
it loads and holds on. It shows it as
`/sys/devices/platform/asustor/lcd_power` (`1`/`0`); older driver builds
had it as the LED `/sys/class/leds/power:lcd` instead, which no longer
exists.

Cutting it **power-cycles the LCD's own MCU, not just a backlight** --
confirmed live: zero serial frames (including a button press) arrive from
the panel for as long as it's off. That makes it unusable for this
project's actual goal: a schedule-driven "night mode" that a button press
can still interrupt. So `lcm-status` never writes it; `lcm-status status`
only shows it, under "Front panel" (`LCD power: on (lcd_power = 1, held by
the driver)`).

Instead, `[sleep]` uses the panel's own **display-off command**, `0x11`
with payload `0x00` -- the same command whose `0x01` form is init step 1.
Found by disassembling ADM's `lcmd`, which sends exactly this for its own
idle timeout, and confirmed live (2026-10-02): the display goes fully
dark, backlight included, while the MCU stays powered; a button press
still arrives as its normal key code, and the MCU switches the display
back on by itself. That first press only wakes the panel -- it's never
also acted on as UP/DOWN/ENTER. `[sleep] lcd_off = false` goes back to
the older behavior of only blanking both lines (a dark-but-not-black
backlight glow).

Night mode only changes what's *shown*: temperature, pool, SMART and fan
monitoring (and the alerts, syslog entries and red bay LEDs they drive)
keep running all night. (Before 2026-10-02 they didn't -- every check
paused from sleep start to wake.)

The daemon only resends a line when its text changes, so it tracks what
the panel is showing. To keep that from drifting -- e.g. the panel's MCU
resetting and showing its own boot text -- both lines are resent every
60s regardless, and when the MCU reports its firmware version (which it
does on boot) the init sequence is redone and everything redrawn.

## Front LEDs

Everything in `led.rs` is a thin wrapper over the standard Linux LED class
sysfs interface (`brightness`, `trigger`, `delay_on`, `delay_off`) that
`asustor-platform-driver` exposes, plus two things that aren't LED class
devices at all:

- **Brightness** (`[led] brightness`, `night_brightness`, 0-100): the
  front LEDs share one brightness control, the IT8625E's PWM3 output.
  Found from ADM's `Hal_Led_Set_Brightness` (its 0-100% slider writes
  `255 - level` to that output's duty; ADM's default is 30%) and confirmed
  live by sweeping it: the power, status, LAN and USB LEDs fade smoothly.
  **The bay LEDs aren't on it** -- they have fixed brightness. The fork's
  vendored `it87`, loaded with **`led_pwm=3 led_pwm_invert=1`**, turns
  that output into an LED class device, `/sys/class/leds/front_panel::brightness`
  (`brightness` 0 = off to `max_brightness` 255, not inverted, kept in
  manual mode by the driver), and there's no hwmon `pwm3` any more. The
  daemon writes `round(percent * max_brightness / 100)` there. Without
  that LED (`it87` loaded without `led_pwm=3`) brightness isn't set at
  all: one WARNING says so, and there's deliberately no fallback to
  `pwm3`. Unset, the LED is never touched (the BIOS leaves it at 204, 80%);
  `lcm-status status` shows the current level under "Front panel".
  `night_brightness` changes what night mode does to those LEDs: instead
  of switching them off, they keep showing status at that level (bay
  green LEDs still go dark). A chassis `LOCATE` at night uses the day
  level. (`lcm-status fan-profile` used to skip `pwm3`; with `led_pwm=3`
  it no longer exists, so there's nothing to skip.)
- **Bay LED style** (`[led] bay_mode`): `"ready"` (factory -- solid green
  while a disk is present, blinks *off* on access) or `"activity"` (dark,
  flashes *on* access), through the platform driver's `disk_led_ready`
  parameter. Applied at startup, only if it differs. Needs the driver's
  "Make `disk_led_ready` writable at runtime" change; an older driver only
  reads it at load, and changing it there means reloading `asustor`, which
  reboots the LCD -- so on an older driver this logs a warning instead.

**Status LED patterns:**

| Pattern | green | red | when |
|---|---|---|---|
| `Ok` | solid | off | nothing wrong |
| `NetworkDown` | solid | solid (= amber) | a physical NIC has no carrier |
| `Degraded` | solid | 500ms/500ms blink | a ZFS pool is `DEGRADED` (factory pattern) |
| `Failed` | off | solid | a pool is `FAULTED`/`UNAVAIL`/`OFFLINE`, or `error`-level socket alert |
| `CriticalFlashing` | off | 1000ms/1000ms blink | `critical`-level socket alert |
| `Locate` | 250ms/250ms blink | 250ms/250ms blink (together = amber flash) | a chassis `LOCATE` (not a health state; wins over all of the above while it lasts) |

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
| `Locate` | a `LOCATE bay=N` is active -- green **and** red flashing together at 250ms/250ms (amber flash); see "`LOCATE`" below |

**Precedence**, highest first: `Locate` > `Alert` > the SMART-derived
state (`Failed`/`Standby`/`Normal`). It's recomputed from all three every
time any one changes rather than remembered, so ending a locate or alert
shows what the layers below it say *now* -- a SMART failure found mid-locate
is solid red the moment the locate ends, not whatever the bay was showing
when it started.

**Front LAN LEDs** (`[led] nic_mode`) aren't GPIOs at all: they're driven
by the RTL8125 PHYs themselves, exposed by `r8169` as
`enpXs0-{0..3}::lan` and configured through the kernel's
hardware-offloaded `netdev` trigger. Channels `-0` (2.5G link) and `-1`
(10/100/1000 link) are OR'd onto each port's front LED; `-2`/`-3`
(presumably the rear RJ45) are left alone. Applied to every NIC that has
these LEDs -- both RTL8125s (`enp2s0`, `enp3s0`), not the AQC113:

| `nic_mode` | `-0` | `-1` | Looks like |
|---|---|---|---|
| `link` (default) | `link_2500` | `link_10 link_100 link_1000` | solid on link at any speed (factory) |
| `activity` | `rx tx` | all off | dark at idle, flickers on traffic |
| *(night mode)* | all off | all off | dark (the `blue:lan` rail is off too) |

Wake from night mode restores the configured `nic_mode`, not a fixed
"link". Things that bit this before:

- **`ledtrig-netdev` isn't loaded by default on TrueNAS** (nor is
  `ledtrig-timer`, which every blink pattern needs). Without it `netdev`
  can't be selected, none of its attributes exist, and every write
  failed silently -- the LEDs just stayed in whatever the PHY had from
  boot. The daemon now `modprobe`s both at startup (as does `deploy.sh`),
  and checks the result by reading back each attribute plus `offloaded`
  (1 = the PHY really is driving the LED in that mode), logging a syslog
  WARNING once per distinct failure (and a notice when it clears) instead
  of ignoring it.
- **r8169 can only offload `rx` and `tx` together.** Changing one alone
  is an unsupported mode: the kernel rejects that write with EOPNOTSUPP
  (keeping the bit, so the matching write completes a valid mode) and
  r8169 switches the LED off for that instant. So `rx` and `tx` are
  always written back to back, after the link bits, never with anything
  in between, and success is judged by the read-back, not by each write.
  Attributes already at the wanted value aren't rewritten, so re-applying
  a mode doesn't blip the LED.

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

**Fixed mode** (`mode = "fixed"` + `fixed_pwm`, the equivalent of ADM's
fixed fan mode) skips the curve and runs the fan at a constant PWM --
with one exception that isn't optional: while any sensor feeding that fan
is at or above its **critical** threshold (its `[[temperature.thresholds]]`
entry, else `[temperature].critical_threshold` -- the same thresholds the
health monitor alerts on), the fan goes to `max_pwm`, and stays there until
*every* sensor is back below its **warning** threshold. A quiet fixed
speed that ignored a drive cooking at 60C would be worse than no fan
control at all, which is also why fixed mode refuses to start without
`sensors`. The gap between critical and warning is deliberate hysteresis:
a drive hovering at its limit doesn't flip the fan between quiet and full
speed, and something that got that hot gets properly cooled, not nudged
just under the line. Both transitions are logged (WARNING on, NOTICE off)
and shown on the fan's `status` line. `fixed_pwm` must be within
`min_pwm..=max_pwm`, or the fan is disabled at load with a diagnostic,
same as an inconsistent curve. Everything below applies to fixed mode
too.

```toml
[[fans]]
name = "chassis"
pwm_chip = "it8625"
fan_index = 1
mode = "fixed"
fixed_pwm = 100        # ~40%; must be within min_pwm..=max_pwm
# ...plus the same [[fans.sensors]] as before -- required in fixed mode
```

Failure handling, all of it aimed at never leaving a fan stopped or
frozen with nothing watching temperatures:

- Fan control runs on its own thread, so a slow or hung `zpool`/`smartctl`
  on the main loop can't stall it (and every external command is killed
  after 10s anyway).
- If every sensor feeding a fan stops reading after the daemon has taken
  it over, the fan is held at `max_pwm` until a reading comes back.
- `pwmN_enable` is set to manual before *every* write. In automatic mode
  the it87 driver rejects pwm writes, and on this board automatic mode
  stops the fan entirely (0 RPM).
- On exit (SIGTERM, `systemctl stop`, a panic) each controlled fan is
  left in manual mode at full speed, not handed back to automatic -- see
  the previous point. The next start takes it straight back over.
- `min_pwm = 0` is allowed: the fan stops below `min_temp_c` and is kicked
  with `min_start_pwm` when the curve next wants it spinning. Only a fan
  that was commanded to spin but whose tach reads 0 counts as stalled.

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
   -- they do nothing at all when actually tested causally. (With `it87`
   loaded with `led_pwm=3`, as this daemon needs, `pwm3` is the front LED
   brightness LED instead and isn't listed at all.)
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

## Buzzer

```toml
[buzzer]
enabled = false   # master switch; off by default
boot = true       # long beep when the daemon starts during boot
power = true      # short beep before a shutdown/restart (also when the OS reboots)
alerts = true     # short beep on warn/error/critical alerts (max 1/min; only critical while asleep)
find_me = true    # short beep when a chassis LOCATE starts
```

Same sounds as ADM, a ~2 kHz tone: one long beep (800 ms) when the daemon
starts during boot, one short beep (200 ms) before a shutdown/restart,
when a chassis LOCATE starts, and on alerts.

**Requirements:** the asustor-platform-driver fork (`main`, v0.3 or later).
The speaker sits behind Super I/O pin GP75; the driver claims that pin
and registers its own buzzer input device, **"ASUSTOR Buzzer"** (phys
`asustor/input0`), which plays tones on the PC speaker and opens the gate
while one plays. `cat /sys/devices/platform/asustor/buzzer_gate` should
say `active`. The stock `pcspkr` module isn't needed (if it's loaded, its
"PC Speaker" device stays silent: the gate only opens for the driver's).

Each beep is an `EV_SND`/`SND_TONE` 2000 Hz event to the ASUSTOR Buzzer's
`/dev/input/eventN` (found by name in `/proc/bus/input/devices`, else
`/sys/class/input`), a wait, and `SND_TONE` 0 (always sent). The daemon
never touches GP75 itself.

Otherwise it doesn't beep, and logs one warning saying why
(`journalctl -u lcm-status | grep buzzer`; `lcm-status status` shows the
same under "Buzzer"):

| Warning says | Do |
|---|---|
| no buzzer gate ... needs the asustor-platform-driver fork | install the fork's driver (`main`, v0.3 or later) |
| buzzer gate is disabled (`buzzer=0`) | reload `asustor.ko` without `buzzer=0` |
| could not claim GP75 ... stale `/sys/class/gpio` export (`it87_gp75`) | unexport it and reload `asustor.ko` (the driver's `deploy.sh` does both) |
| `buzzer_gate` is active but there is no "ASUSTOR Buzzer" input device | the loaded `asustor.ko` is a fork build from before v0.3 that gated `pcspkr` instead; update the driver and reload it |

Both are re-checked before every beep, so fixing either takes effect
without restarting the daemon (logged when the buzzer becomes ready).

## Rear reset button (not used)

The pinhole button on the back is a plain GPIO input, not a hardware
reset; ADM restores settings after it's held for ~5 s. With the platform
driver fork (v0.3 or later) it shows up as `KEY_VENDOR` (code 360) on the
`asustor-keys` input device, next to the USB Copy button
(`sudo evtest`, pick "asustor-keys"). `lcm-status` doesn't read it and
nothing else on the system acts on it -- deliberately not `KEY_RESTART`,
which systemd-logind would turn into an instant reboot. Pressing it is
harmless; it's there if a "hold for N seconds" action is ever wanted.

## Wake-on-LAN

```toml
[wol]
nics = ["enp2s0"]   # default [] = WOL left however the driver set it
mode = "g"          # ethtool notation; "g" (default) = magic packet
```

Neither TrueNAS nor the kernel turns WOL on by itself (`ethtool enp2s0`
shows `Supports Wake-on: pumbg`, `Wake-on: d` on a fresh boot), and it
doesn't persist across reboots. `wol.rs` sets it straight through the
`SIOCETHTOOL` ioctl (`ETHTOOL_GWOL`/`ETHTOOL_SWOL` -- the same thing
`ethtool -s enp2s0 wol g` does, without depending on `ethtool` being
installed), and keeps it set:

- at daemon startup;
- every 60s after that (one cheap ioctl per NIC), in case a link change
  or driver reload reset it -- logged as a NOTICE when that happens;
- once more as the daemon exits, which during a shutdown is shortly
  before power-off.

`mode` *replaces* the NIC's setting rather than adding to it, like
`ethtool` -- so `"d"` keeps WOL off. Any of `p`/`u`/`m`/`b`/`a`/`g` are
accepted; `s` (SecureOn password) and `f` (filters) aren't, since there's
nowhere to configure what they need. A mode the NIC doesn't support, or a
driver that doesn't take it, is logged once as a WARNING (and once more
when it recovers). The `status` report has a Wake-on-LAN section listing
every physical NIC's current mode and what it supports, whether or not
it's in `nics` -- handy for checking a NIC before adding it.

Caveats -- the NIC only arms WOL; whether the box actually powers on is
up to the board:

- **BIOS:** ErP/EuP (deep power saving) must be **off**, and wake on
  PCIe/PCI-E device or LAN **enabled**. With ErP on, the NICs lose standby
  power in soft-off and nothing can wake the box.
  With the platform driver fork (v0.3 or later), the daemon reads
  `/sys/devices/platform/asustor/eup` (never writes it) and logs a WARNING
  at startup if EuP is on while `[wol] nics` or a `power_on` rule needs
  wake from soft-off; `lcm-status status` shows it (and
  `ac_power_resume`) under "Platform power (BIOS)".
- Works from **soft-off** (a normal shutdown -- the panel's SHUTDOWN, the
  TrueNAS UI, `poweroff`) and from suspend. Not after the power has been
  cut entirely (unplugged, a power failure) -- that's the BIOS's "restore
  on AC power loss" setting's job instead.
- The magic packet has to reach that port: sent to *that NIC's* MAC, on
  its broadcast domain, with a cable connected at the time of shutdown.

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

## Power schedule

ADM's Control Panel -> Hardware -> Power -> Power Schedule, rebuilt for
TrueNAS: weekly rules to power on, shut down or restart the NAS at set
times. Configured as `[[power_schedule]]` blocks in `lcm-status.toml`:

```toml
[[power_schedule]]
days = "weekdays"          # or ["mon", "wed"], "daily", "weekends", ...
time = "07:30"             # 24h, local time
action = "power_on"

[[power_schedule]]
days = "daily"
time = "23:30"
action = "shutdown"        # or "restart"; ADM's "power_off" also works
countdown_secs = 120       # front-panel countdown first (default 60)
```

`days` takes `sun`..`sat` (or full names) plus the shorthands `daily`,
`weekdays` (Mon-Fri) and `weekends`, as a list or a single string. A bad
entry (unknown day or action, unparseable time) is reported at load
(`lcm-status check-config`) and disabled; the rest of the schedule still
applies. Combinations that won't do what was meant are reported but kept:
a shutdown and a restart in the same minute (the shutdown wins), and a
power on less than ~5 minutes plus the countdown after a shutdown (the
alarm can go off before the NAS is actually off, and is then spent).

**How ADM does it** (from `emboardmand`'s `Power_Service_Handler` thread
and `libndal.so`/`hardware.js`): rules are `{type, days, hour, minute}`,
`type` one of `power_on`/`power_off`/`restart`/`sleep`, days numbered
0 = Sunday (stored as `[Power Schedule N]` sections in `emboard.conf`).
Once a minute it compares the clock to every rule; a matching power
off/restart happens immediately (skipped while an md array is reshaping),
and the next power on is kept written into the RTC wake alarm. The
semantics here match, except:

- **Power on** is the RTC wake alarm, `/sys/class/rtc/rtc0/wakealarm`.
  Whenever at least one `power_on` rule exists the daemon owns that alarm
  and keeps it set to the next scheduled power on at all times -- set at
  startup, re-checked every minute (so a manual `rtcwake` gets
  overwritten), moved on as soon as one passes, after any clock jump,
  right before any shutdown/restart it runs, and on exit. So **any**
  shutdown -- scheduled, front-panel menu, TrueNAS UI, `poweroff` over SSH
  -- wakes at the next scheduled time. With no `power_on` rule the alarm
  is never touched. The alarm takes UTC epoch seconds regardless of
  whether the RTC keeps UTC or local time; the daemon finds the next
  matching *local* minute with libc's `localtime_r`, so DST is handled: a
  time that doesn't exist on a spring-forward day is skipped that day, and
  one that happens twice on a fall-back day only counts the first time.
- **Shutdown/restart** go through the same `systemctl poweroff`/`reboot`
  as the front-panel menu, after an LCD countdown (`SHUTDOWN IN 0:42` /
  `ANY KEY: CANCEL`) that any button cancels. The countdown wakes the
  panel from night mode and takes over from the action menu; socket
  alerts that arrive meanwhile are held, like during a confirm screen.
  Everything is logged to syslog (due, cancelled, executed).
- **Firing is edge-triggered**: a rule fires when its minute boundary is
  crossed while the daemon is running, never because the current minute
  matches. A daemon that starts (or restarts) inside or after a scheduled
  minute doesn't fire it, so a restart rule can't reboot-loop on a fast
  boot and a crash-restart can't shut down twice. A clock step of more
  than 5 minutes either way (NTP fixing a bad clock) fires nothing it
  skipped over; a small step back doesn't refire what already ran.
- No `sleep` rule type (TrueNAS doesn't suspend), and no "skip while the
  array is rebuilding" -- a ZFS resilver or scrub resumes after a reboot.

**Caveats**, from the hardware rather than this daemon:

- **BIOS ErP/EuP must be off** (ADM tracks this as `EuPMode` in
  `emboard.conf`). With it on, the board cuts standby power in soft-off
  and nothing -- RTC alarm or Wake-on-LAN -- can wake it.
- A power on only works from **soft-off (S5)**: a normal shutdown. After
  an AC power loss the board isn't waiting on its RTC alarm; what happens
  when power returns is the BIOS's restore-on-AC-loss setting.
- `rtc0` must support alarms a week ahead. This box's does (`cat
  /proc/driver/rtc` shows an alarm date, not just a time); one that only
  takes 24h alarms makes setting it fail with "Invalid argument", which is
  logged.

The next scheduled power on/off (and what the RTC alarm is actually set
to) shows in `lcm-status status` and, computed fresh, in `lcm-status
check-config`.

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

LOCATE [bay=N] [ttl_secs]
LOCATE off [bay=N]
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
- An active override **never** interrupts the button-driven action menu,
  a shutdown/restart confirm screen, or a power-schedule countdown --
  it's queued (keeping the most
  severe if several arrive) and applied the instant the user backs out or
  confirms, subject to the same rule that a lower level never replaces a
  more severe active alert.

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

### `LOCATE`: find a drive (or the box)

The equivalent of ADM's "inspection LED": blink something distinctive so
whoever is standing at the rack can find it.

```sh
sudo lcm-status locate 2              # bay 2, default 60s
sudo lcm-status locate 2 --ttl 300    # bay 2, 5 minutes
sudo lcm-status locate                # the whole chassis
sudo lcm-status locate --ttl 0        # chassis, until turned off
sudo lcm-status locate 2 --off        # stop bay 2
sudo lcm-status locate --off          # stop every locate
```

(`[CONFIG]` can follow, as for `status`, to find a non-default socket
path.) These just send `LOCATE [off] [bay=N] [ttl_secs]` over the socket,
so `printf "LOCATE bay=2\n" | nc -U /run/lcm-status.sock -q1` is the same
thing.

- **`bay=N`**: both of that bay's LEDs flash together at 250ms/250ms --
  amber, not red or green. No other bay state ever lights both colors at
  once, and the rate is unlike all of them too (`Alert` is 1000/1000 red,
  `Standby` is a 250/9750 green blip). Strict green/red *alternation*
  isn't possible: each LED's `ledtrig-timer` blink is its own software
  timer, restarted by every `delay_on`/`delay_off` write, with no way to
  set one's phase against another's -- written back to back, they start
  in step instead. Bays count from 1; an empty bay can be located too.
- **No bay**: the whole chassis -- the blue power LED and both status LED
  colors flash at the same 250/250, and the LCD shows the hostname over
  `LOCATE 57s` (the time left). The power LED never blinks for anything
  else.
- **`ttl_secs`**: how long it lasts; default 60, `0` = until `LOCATE off`.
  Sending `LOCATE` again for something already blinking restarts its TTL.
- **`LOCATE off bay=N`** stops that bay; **`LOCATE off`** with no bay
  stops *everything* being located, chassis and bays alike.
- **`CLEAR` doesn't touch a locate**, in either direction: the two are
  independent, so a script clearing its own alert can't cut someone's
  locate short.

While it lasts, a locate wins over everything else on the LEDs it uses,
including a confirmed SMART failure (the drive you're hunting for is often
the failed one -- which is why its pattern has to be distinguishable from
`Failed`) and a `critical` override's status-LED flash and LCD text. When it
ends, those LEDs (and the LCD) go back to whatever the daemon's state says
they should show *at that moment* -- see "Precedence" under Front LEDs.
The one exception: a chassis locate never interrupts the action menu or a
shutdown/restart confirm screen on the LCD (it waits behind them, like an
override) -- its LEDs still start at once.

**At night**, a locate lights its LEDs anyway without waking the rest of
the panel -- NIC/LAN/USB LEDs stay dark, and a chassis locate shows only
its own screen on the otherwise-blank LCD. When it ends, its LEDs go back
to the night state (bay green dark, red still showing a real failure;
power and status dark), not the daytime one. If night mode starts or ends
while a locate is running, the locate keeps blinking straight through.

`lcm-status status` lists active locates and their time left under
`-- Locate --`.

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
NIC's link/monitoring status and Wake-on-LAN state, plus the power
schedule's next events and RTC wake alarm (and a running shutdown
countdown, if any), the LCD power rail, whether the buzzer can beep (and if not, why), and
the active socket override if any:

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

-- Wake-on-LAN --
  enp2s0     g      (supports pumbg)  managed: keeping at g
  enp3s0     d      (supports pumbg)
  enp9s0     g      (supports pg)

-- Power schedule --
  next power on:   Mon 2026-10-05 07:30 (in 2d 14h 55m, #1 power on weekdays 07:30)
  next power off:  Fri 2026-10-02 23:30 (in 6h 55m, #3 shutdown daily 23:30)
  RTC wake alarm:  set for Mon 2026-10-05 07:30

-- Front panel --
  LCD power:      on (lcd_power = 1, held by the driver)
  LED brightness: 30% (77/255, front_panel::brightness)

-- Buzzer --
  ready: 2000 Hz tones to /dev/input/event7 (ASUSTOR Buzzer)

-- LCD link --
  412 text writes, 0 retried, 0 failed

-- Active override --
  none

-- Locate --
  bay 2 (41s left)

-- Overall status LED --
  pattern: Ok
  severity: Info  (fan=Info temp=Info network=Info pool_degraded=false pool_faulted=false bay_failed=false)
```

Implementation note, if you're extending the protocol further: `handle_connection`
keeps a second cloned handle to the stream for writing the response after
the `BufReader` has consumed the original for reading -- a Unix stream
socket's two directions are independent, so this works even though a
plain read loop would otherwise "consume" the connection. The client
(`socket_request` in `main.rs`) sends its request, then shuts down just
its *write* half (`Shutdown::Write`) so the daemon's `.lines()` sees EOF
and stops waiting for more input, while the read half stays open to
receive the reply.

## Config

`/etc/lcm-status.toml`, hand-edited, every field defaulted -- see
`lcm-status.example.toml` for the full annotated reference (scroll
speed/limits, per-category refresh floors, sleep schedule, power
schedule, which screens are enabled and in what order, NIC LED mode,
Wake-on-LAN, Docker containers to ignore, temperature units/warning
threshold).

The text on each status screen comes from `[templates.*]`: `{variable}`
templates per screen kind (`network`, `pool`, `hdd`, `cpu`, `fan`,
`docker`), each with its own variable list documented in the example
config. The defaults reproduce the built-in text. A block that sets only
one line leaves the other line blank; leave the block out entirely to keep
its defaults. Templates are checked at
load: one with an unknown variable or a stray brace is logged and falls
back to its default, without affecting the other screens. Fallback screens
("no disks found"), socket overrides and the action/confirm menu aren't
templated. `lcm-status hal-test [path]` renders every screen using that
config, so you can preview edits before restarting the daemon.

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
lcm-status daemon [path]           # run the daemon (what the systemd unit does)
lcm-status status [path]           # the running daemon's health report
lcm-status locate [bay] [--ttl N] [--off] [path]
                                    # blink a bay's (or the chassis') LEDs to find it
lcm-status init                    # send the power-on sequence
lcm-status settext 0 "HELLO"       # write up to 16 chars to line 0 or 1
lcm-status listen 60               # print every unsolicited frame (button
                                    # presses, MCU version reports) for 60s
lcm-status hal-test [path]         # dump every screen, rendered with that config's templates
lcm-status check-config [path]     # parse and print a config file, and
                                    # when its power schedule next fires
lcm-status --help                  # usage
```

An unrecognized subcommand is an error (exit status 2), not a config
path. Only `daemon` (or a bare argument that looks like a path, the
form older unit files used) starts the daemon, and the daemon refuses to
start if another instance is already listening on the socket.
