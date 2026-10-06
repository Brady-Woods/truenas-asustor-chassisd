//! The chassis beeper, ADM-style: a 2 kHz tone from the platform driver's
//! own buzzer input device ("ASUSTOR Buzzer").
//!
//! The speaker (the PC speaker, PIT channel 2) sits behind a gate, Super
//! I/O pin GP75: with it low, nothing is audible (which is why the stock
//! `pcspkr` is silent on the AS6704T). The forked `asustor.ko` (v0.3 or
//! later) claims GP75 itself and registers an input device,
//! "ASUSTOR Buzzer" (phys `asustor/input0`), that plays tones on the PC
//! speaker and opens the gate while one plays. It reports whether that
//! works in `buzzer_gate` (`active`, `disabled` with `buzzer=0`,
//! `unavailable` when it couldn't claim GP75). So a beep is just
//! `SND_TONE` 2000 Hz, a wait, `SND_TONE` 0 -- the kernel times the square
//! wave and the daemon never touches GP75. `pcspkr` isn't involved.
//!
//! Without that gate, or without the device, there is no beeping: one
//! warning names the reason. Both are re-checked before every beep (a
//! read and a stat or two), so a driver loaded later is picked up without
//! a restart (and logged).
//!
//! Beeps play on a worker thread so the event loop never waits out an
//! 800 ms tone; patterns requested while one is playing queue behind it.

use crate::config::BuzzerConfig;
use crate::syslog;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Duration;

/// ADM's pitch.
const TONE_HZ: i32 = 2000;

/// The platform driver's buzzer input device (`/dev/input/by-path/
/// platform-asustor-event` under udev's usual rules, but found by name).
const BUZZER_NAME: &str = "ASUSTOR Buzzer";

/// From `<linux/input-event-codes.h>`.
const EV_SND: u16 = 0x12;
const SND_TONE: u16 = 0x02;
/// `struct input_event`: a `struct timeval` (ignored on write), then
/// `__u16 type`, `__u16 code`, `__s32 value` -- 24 bytes on `x86_64`.
const TIMEVAL_SIZE: usize = std::mem::size_of::<libc::timeval>();
const INPUT_EVENT_SIZE: usize = TIMEVAL_SIZE + 8;

/// ADM's two patterns: long once boot has finished, short for shutdown,
/// restart, warnings and errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Beep {
    Short,
    Long,
}

impl Beep {
    fn duration(self) -> Duration {
        match self {
            Beep::Short => Duration::from_millis(200),
            Beep::Long => Duration::from_millis(800),
        }
    }
}

static GLOBAL: OnceLock<(Buzzer, BuzzerConfig)> = OnceLock::new();

/// Starts the beeper for the life of the process (no thread when disabled).
pub fn init(cfg: &BuzzerConfig) {
    let _ = GLOBAL.set((Buzzer::new(cfg.enabled), cfg.clone()));
}

fn play_if(wanted: impl Fn(&BuzzerConfig) -> bool, beep: Beep) {
    if let Some((buzzer, cfg)) = GLOBAL.get()
        && wanted(cfg)
    {
        buzzer.beep(beep);
    }
}

/// The daemon came up during boot: one long beep.
pub fn boot_finished() {
    play_if(|c| c.boot, Beep::Long);
}

/// Blocks until every queued beep has finished. Call before exiting: the
/// process can then exit (and the machine power off) without cutting one
/// short or leaving a tone playing.
pub fn flush() {
    if let Some((Buzzer { tx: Some(tx) }, _)) = GLOBAL.get() {
        let (done, wait) = mpsc::channel();
        if tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

/// A shutdown or restart is about to run.
pub fn powering_down() {
    play_if(|c| c.power, Beep::Short);
}

/// A chassis `LOCATE` started.
pub fn find_me() {
    play_if(|c| c.find_me, Beep::Short);
}

/// A warning/error/critical alert was raised. The caller rate-limits.
pub fn alert() {
    play_if(|c| c.alerts, Beep::Short);
}

/// One line for `lcm-status status`: whether a beep would sound now, and
/// if not, why.
pub fn status(cfg: &BuzzerConfig) -> String {
    match Paths::system().check(cfg.enabled) {
        Ok(device) => ready_line(&device),
        Err(reason) => format!("not beeping: {reason}"),
    }
}

fn ready_line(device: &Path) -> String {
    format!(
        "ready: {TONE_HZ} Hz tones to {} ({BUZZER_NAME})",
        device.display()
    )
}

enum Msg {
    Beep(Beep),
    /// Answered once every earlier beep has finished playing.
    Flush(mpsc::Sender<()>),
}

/// Handle to the beeper thread. Silent (and free) when disabled.
struct Buzzer {
    tx: Option<mpsc::Sender<Msg>>,
}

impl Buzzer {
    fn new(enabled: bool) -> Self {
        if !enabled {
            return Buzzer { tx: None };
        }
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("buzzer".into())
            .spawn(move || worker(&rx));
        if let Err(e) = spawned {
            syslog::warning(&format!("buzzer: could not start thread: {e}"));
            return Buzzer { tx: None };
        }
        Buzzer { tx: Some(tx) }
    }

    fn beep(&self, beep: Beep) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Beep(beep));
        }
    }
}

fn worker(rx: &mpsc::Receiver<Msg>) {
    let paths = Paths::system();
    // Logged at startup, then again only when it changes.
    let mut announced: Option<Result<PathBuf, String>> = None;
    let mut check = || {
        let now = paths.check(true);
        if announced.as_ref() != Some(&now) {
            match &now {
                Ok(device) => syslog::info(&format!("buzzer: {}", ready_line(device))),
                Err(reason) => syslog::warning(&format!("buzzer: not beeping: {reason}")),
            }
            announced = Some(now.clone());
        }
        now
    };
    let _ = check();

    let mut failed = false;
    while let Ok(msg) = rx.recv() {
        let beep = match msg {
            Msg::Beep(beep) => beep,
            Msg::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        let Ok(device) = check() else { continue };
        let played = OpenOptions::new()
            .write(true)
            .open(&device)
            .and_then(|mut speaker| play(&mut speaker, beep.duration()));
        match played {
            Ok(()) => failed = false,
            Err(e) => {
                if !failed {
                    syslog::warning(&format!("buzzer: {}: {e}", device.display()));
                }
                failed = true;
            }
        }
        // A burst of requests would otherwise run together into one tone.
        thread::sleep(Duration::from_millis(100));
    }
}

/// The driver's `buzzer_gate` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gate {
    /// The driver's buzzer device is registered and opens GP75 while it
    /// plays a tone.
    Active,
    /// `asustor.ko` loaded with `buzzer=0`.
    Disabled,
    /// The driver couldn't claim GP75.
    Unavailable,
    /// No such file: an older driver, or a model without a gate.
    Absent,
    /// Anything else (unreadable, or a value this version doesn't know),
    /// as shown in the warning.
    Other(String),
}

impl Gate {
    fn parse(text: &str) -> Gate {
        match text.trim() {
            "active" => Gate::Active,
            "disabled" => Gate::Disabled,
            "unavailable" => Gate::Unavailable,
            other => Gate::Other(format!("\"{other}\"")),
        }
    }

    fn read(file: &Path) -> Gate {
        match std::fs::read_to_string(file) {
            Ok(text) => Gate::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Gate::Absent,
            Err(e) => Gate::Other(format!("unreadable ({e})")),
        }
    }
}

/// The speaker device to send tones to, or why a beep wouldn't sound.
fn availability(enabled: bool, gate: &Gate, speaker: Option<PathBuf>) -> Result<PathBuf, String> {
    if !enabled {
        return Err("turned off ([buzzer] enabled = false)".into());
    }
    match gate {
        Gate::Active => speaker.ok_or_else(|| {
            format!(
                "buzzer_gate is active but there is no \"{BUZZER_NAME}\" input device: \
                 the asustor driver is too old (a fork build from before v0.3 that gated \
                 pcspkr instead); update it and reload asustor.ko"
            )
        }),
        Gate::Absent => Err(format!(
            "the platform driver has no buzzer gate (no buzzer_gate attribute); needs the \
             asustor-platform-driver fork (https://github.com/Brady-Woods/asustor-platform-driver), \
             main, v0.3 or later, with its \"{BUZZER_NAME}\" device"
        )),
        Gate::Disabled => Err("the platform driver's buzzer gate is disabled (asustor.ko \
                               loaded with buzzer=0)"
            .into()),
        Gate::Unavailable => Err("the platform driver could not claim GP75 for its buzzer \
                                  gate, most likely because of a stale /sys/class/gpio export \
                                  (it87_gp75) when asustor.ko loaded; unexport it and reload \
                                  asustor.ko"
            .into()),
        Gate::Other(value) => Err(format!(
            "the platform driver's buzzer_gate is {value}, not \"active\""
        )),
    }
}

/// Where the gate and speaker show up; tests point these at a scratch
/// directory.
struct Paths {
    gate: PathBuf,
    input_devices: PathBuf,
    input_class: PathBuf,
    dev_input: PathBuf,
}

impl Paths {
    fn system() -> Self {
        Paths {
            gate: "/sys/devices/platform/asustor/buzzer_gate".into(),
            input_devices: "/proc/bus/input/devices".into(),
            input_class: "/sys/class/input".into(),
            dev_input: "/dev/input".into(),
        }
    }

    fn check(&self, enabled: bool) -> Result<PathBuf, String> {
        availability(enabled, &Gate::read(&self.gate), self.buzzer_device())
    }

    /// The `/dev/input/eventN` node of the driver's buzzer device, if
    /// registered.
    fn buzzer_device(&self) -> Option<PathBuf> {
        let event = std::fs::read_to_string(&self.input_devices)
            .ok()
            .and_then(|text| buzzer_handler(&text))
            .or_else(|| buzzer_in_class(&self.input_class))?;
        Some(self.dev_input.join(event))
    }
}

/// The `eventN` handler of the "ASUSTOR Buzzer" entry in
/// `/proc/bus/input/devices` text (blank-line separated blocks of `N:
/// Name="..."` and `H: Handlers=kbd event5` lines, among others).
fn buzzer_handler(text: &str) -> Option<String> {
    let mut is_buzzer = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            is_buzzer = false;
        } else if let Some(name) = line.strip_prefix("N: Name=") {
            is_buzzer = name.trim().trim_matches('"') == BUZZER_NAME;
        } else if is_buzzer && let Some(handlers) = line.strip_prefix("H: Handlers=") {
            return handlers
                .split_whitespace()
                .find(|h| is_event_node(h))
                .map(str::to_string);
        }
    }
    None
}

/// The same, from `/sys/class/input/eventN/device/name`.
fn buzzer_in_class(class: &Path) -> Option<String> {
    let mut events: Vec<String> = std::fs::read_dir(class)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| is_event_node(n))
        .collect();
    events.sort();
    events.into_iter().find(|n| {
        std::fs::read_to_string(class.join(n).join("device/name"))
            .is_ok_and(|name| name.trim() == BUZZER_NAME)
    })
}

fn is_event_node(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `struct input_event` bytes; the timestamp stays zero (the kernel
/// ignores it on writes).
fn input_event(kind: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_SIZE] {
    let mut event = [0u8; INPUT_EVENT_SIZE];
    event[TIMEVAL_SIZE..TIMEVAL_SIZE + 2].copy_from_slice(&kind.to_ne_bytes());
    event[TIMEVAL_SIZE + 2..TIMEVAL_SIZE + 4].copy_from_slice(&code.to_ne_bytes());
    event[TIMEVAL_SIZE + 4..].copy_from_slice(&value.to_ne_bytes());
    event
}

/// Asks the speaker device for a tone of `hz` (0 = stop). One whole event
/// per `write`, which is what evdev takes.
fn send_tone(speaker: &mut impl Write, hz: i32) -> io::Result<()> {
    let event = input_event(EV_SND, SND_TONE, hz);
    match speaker.write(&event)? {
        n if n == event.len() => Ok(()),
        n => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short input_event write ({n} of {} bytes)", event.len()),
        )),
    }
}

/// Tone on, wait, tone off. The tone-off event is sent whatever happened
/// to the tone-on one, so no way out leaves the speaker sounding.
fn play(speaker: &mut impl Write, length: Duration) -> io::Result<()> {
    let started = send_tone(speaker, TONE_HZ);
    if started.is_ok() {
        thread::sleep(length);
    }
    let stopped = send_tone(speaker, 0);
    started.and(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn long_beep_is_four_times_the_short_one() {
        assert_eq!(Beep::Long.duration(), Beep::Short.duration() * 4);
    }

    #[test]
    fn disabled_buzzer_ignores_beeps() {
        Buzzer::new(false).beep(Beep::Short);
    }

    /// A fresh scratch directory per test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lcm-status-buzzer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            gate: root.join("asustor/buzzer_gate"),
            input_devices: root.join("devices"),
            input_class: root.join("input"),
            dev_input: "/dev/input".into(),
        }
    }

    /// With `pcspkr` loaded too: its "PC Speaker" (silent on this board,
    /// the gate only opens for the driver's own device) must never be
    /// taken for the buzzer.
    const PROC_INPUT_DEVICES: &str = "\
I: Bus=0019 Vendor=0000 Product=0001 Version=0000
N: Name=\"Power Button\"
P: Phys=LNXPWRBN/button/input0
H: Handlers=kbd event0
B: EV=3

I: Bus=0010 Vendor=001f Product=0001 Version=0100
N: Name=\"PC Speaker\"
P: Phys=isa0061/input0
S: Sysfs=/devices/platform/pcspkr/input/input5
U: Uniq=
H: Handlers=kbd event5
B: PROP=0
B: EV=40001
B: SND=6

I: Bus=0019 Vendor=0000 Product=0000 Version=0000
N: Name=\"ASUSTOR Buzzer\"
P: Phys=asustor/input0
S: Sysfs=/devices/platform/asustor/input/input7
U: Uniq=
H: Handlers=kbd event7
B: PROP=0
B: EV=40001
B: SND=6
";

    #[test]
    fn gate_values_are_parsed() {
        assert_eq!(Gate::parse("active\n"), Gate::Active);
        assert_eq!(Gate::parse("active"), Gate::Active);
        assert_eq!(Gate::parse("disabled\n"), Gate::Disabled);
        assert_eq!(Gate::parse("unavailable\n"), Gate::Unavailable);
        // Anything else is not active, and shown as read.
        assert_eq!(Gate::parse("Active\n"), Gate::Other("\"Active\"".into()));
        assert_eq!(Gate::parse(""), Gate::Other("\"\"".into()));
        assert_eq!(Gate::parse("on\n"), Gate::Other("\"on\"".into()));
    }

    #[test]
    fn a_missing_gate_file_is_absent() {
        let root = scratch("gate");
        let p = paths(&root);
        assert_eq!(Gate::read(&p.gate), Gate::Absent);
        write(&p.gate, "active\n");
        assert_eq!(Gate::read(&p.gate), Gate::Active);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn beeps_need_enabled_an_active_gate_and_the_buzzer_device() {
        let device = || Some(PathBuf::from("/dev/input/event7"));
        assert_eq!(
            availability(true, &Gate::Active, device()),
            Ok(PathBuf::from("/dev/input/event7"))
        );
        let reason =
            |enabled, gate: Gate, device| availability(enabled, &gate, device).unwrap_err();
        assert!(reason(false, Gate::Active, device()).contains("enabled = false"));
        // Gate active but no device: a fork driver from before v0.3 that
        // gated pcspkr. The fix is the driver, not pcspkr.
        let old = reason(true, Gate::Active, None);
        assert!(
            old.contains("too old") && old.contains("reload asustor.ko"),
            "{old}"
        );
        assert!(!old.contains("modprobe"), "{old}");
        assert!(reason(true, Gate::Absent, device()).contains("main, v0.3 or later"));
        assert!(reason(true, Gate::Disabled, device()).contains("buzzer=0"));
        let stale = reason(true, Gate::Unavailable, device());
        assert!(stale.contains("it87_gp75") && stale.contains("reload asustor.ko"));
        assert!(reason(true, Gate::parse("on"), device()).contains("\"on\", not \"active\""));
        // Without a gate, that's what's reported, device or not: it's the
        // driver that needs changing first.
        assert!(reason(true, Gate::Absent, None).contains("main, v0.3 or later"));
    }

    #[test]
    fn check_reads_the_gate_and_finds_the_buzzer() {
        let root = scratch("check");
        let p = paths(&root);
        assert!(p.check(true).unwrap_err().contains("main, v0.3 or later"));
        write(&p.gate, "active\n");
        assert!(p.check(true).unwrap_err().contains("too old"));
        write(&p.input_devices, PROC_INPUT_DEVICES);
        assert_eq!(p.check(true), Ok(PathBuf::from("/dev/input/event7")));
        write(&p.gate, "unavailable\n");
        assert!(p.check(true).unwrap_err().contains("it87_gp75"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn buzzer_is_found_in_proc_input_devices() {
        assert_eq!(
            buzzer_handler(PROC_INPUT_DEVICES).as_deref(),
            Some("event7")
        );
        // Only pcspkr's device: not the buzzer.
        let pcspkr_only = PROC_INPUT_DEVICES.replace("ASUSTOR Buzzer", "Sleep Button");
        assert_eq!(buzzer_handler(&pcspkr_only), None);
        assert_eq!(buzzer_handler(""), None);
    }

    #[test]
    fn buzzer_is_found_in_sys_class_input() {
        let root = scratch("class");
        let p = paths(&root);
        write(&p.input_class.join("event0/device/name"), "Power Button\n");
        write(&p.input_class.join("event1/device/name"), "PC Speaker\n");
        write(
            &p.input_class.join("event3/device/name"),
            "ASUSTOR Buzzer\n",
        );
        write(&p.input_class.join("input3/name"), "ASUSTOR Buzzer\n");
        // No /proc file in the scratch tree: falls back to the class dir.
        assert_eq!(p.buzzer_device(), Some(PathBuf::from("/dev/input/event3")));
        // /proc wins when it has an answer.
        write(&p.input_devices, PROC_INPUT_DEVICES);
        assert_eq!(p.buzzer_device(), Some(PathBuf::from("/dev/input/event7")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tone_events_are_input_events() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(INPUT_EVENT_SIZE, 24);
        let event = input_event(EV_SND, SND_TONE, 2000);
        assert!(event[..TIMEVAL_SIZE].iter().all(|&b| b == 0));
        assert_eq!(event[TIMEVAL_SIZE..TIMEVAL_SIZE + 2], 0x12u16.to_ne_bytes());
        assert_eq!(
            event[TIMEVAL_SIZE + 2..TIMEVAL_SIZE + 4],
            2u16.to_ne_bytes()
        );
        assert_eq!(event[TIMEVAL_SIZE + 4..], 2000i32.to_ne_bytes());
    }

    /// A speaker whose first `write` fails; records the rest.
    #[derive(Default)]
    struct FlakySpeaker {
        failed_once: bool,
        written: Vec<u8>,
    }

    impl Write for FlakySpeaker {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.failed_once {
                self.failed_once = true;
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            self.written.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_beep_is_tone_on_then_tone_off() {
        let mut speaker = Vec::new();
        play(&mut speaker, Duration::ZERO).unwrap();
        let mut want = input_event(EV_SND, SND_TONE, TONE_HZ).to_vec();
        want.extend(input_event(EV_SND, SND_TONE, 0));
        assert_eq!(speaker, want);
    }

    #[test]
    fn the_tone_is_stopped_even_if_starting_it_failed() {
        let mut speaker = FlakySpeaker::default();
        assert!(play(&mut speaker, Duration::ZERO).is_err());
        assert_eq!(speaker.written, input_event(EV_SND, SND_TONE, 0));
    }
}
