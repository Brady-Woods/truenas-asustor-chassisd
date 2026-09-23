# lcm-status

A portable, from-scratch driver and status daemon for the ASUSTOR front-panel
LCM (LCD module) and front LEDs, for running ASUSTOR NAS hardware (built
against an AS6704T v2 / LOCKERSTOR 4 Gen2+) under TrueNAS SCALE instead of
ADM. ADM's own `lcmd` binary won't run on TrueNAS at all -- it's linked
against ASUSTOR's proprietary `libgeneral.so`/`libnasman.so`/`libnhal.so`/
`libndal.so`, none of which exist outside ADM, and it needs glibc symbol
versions newer than what TrueNAS ships. This project talks to the same
hardware directly instead of trying to run ADM's binary.

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

Drives `pwm1` (the `it8625` hwmon chip's one populated fan header) from
temperature -- see `src/fan.rs`. This used to be lm-sensors' `fancontrol`
package's job; it's implemented here now because `fancontrol` turned out
not to be part of TrueNAS SCALE's base image after all (see "Deploying,
and surviving TrueNAS upgrades" below), and reimplementing the curve logic
means fan control depends on nothing but this daemon.

The curve algorithm is ported directly from upstream `fancontrol(8)`'s
`UpdateFanSpeeds`, configured under `[fan]` in `lcm-status.toml` with the
same `min_temp_c`/`max_temp_c`/`min_start_pwm`/`min_stop_pwm`/`min_pwm`/
`max_pwm` knobs `/etc/fancontrol` used to have. One difference from
upstream: the control temperature is `max(CPU package temp, every SATA
drive's `drivetemp` reading, every NVMe controller's reading)`, not CPU
alone -- a hot drive ramps the fan even with the CPU idle. CPU temp is
read fresh every `update_secs` (default 1s, matching upstream's default
`INTERVAL`); drive/NVMe temps are resampled only every
`drive_temp_min_secs` (default 30s) since they change slowly and reading
them (`/sys/class/hwmon/*/temp1_input` directly, no `smartctl`) has no
reason to run as often as the CPU check.

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
