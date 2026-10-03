//! The chassis beeper, the way ADM drives it (its `asbuzzer_js.ko`):
//! toggle the two speaker bits of port 0x61 at ~2 kHz, with Super I/O pin
//! GP75 held high for the duration. The stock `pcspkr` driver (a PIT
//! channel 2 tone) is silent on this board -- the firmware leaves that
//! timer's clock gated -- so this goes straight to the port, through
//! `/dev/port`.
//!
//! Beeps play on a worker thread so the event loop never waits out an
//! 800 ms tone; patterns requested while one is playing queue behind it.

use crate::config::BuzzerConfig;
use crate::syslog;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const SPEAKER_PORT: u64 = 0x61;
/// Bit 0 gates PIT channel 2 into the speaker, bit 1 enables it; with the
/// timer out of the picture, flipping both is what makes the cone move.
const SPEAKER_BITS: u8 = 0b11;
/// 250 us each way = a 2 kHz square wave, ADM's pitch.
const HALF_PERIOD: Duration = Duration::from_micros(250);

/// `gpiochip` base 852 (`asustor_gpio_it87`) + GP75's offset of 53.
const GP75_GPIO: &str = "905";
const GP75_DIR: &str = "/sys/class/gpio/it87_gp75/direction";
const GP75_VALUE: &str = "/sys/class/gpio/it87_gp75/value";

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

/// Blocks until every queued beep has finished, so the process can exit
/// (and the machine power off) without cutting one short.
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
    let port = match OpenOptions::new().read(true).write(true).open("/dev/port") {
        Ok(f) => f,
        Err(e) => {
            syslog::warning(&format!("buzzer: cannot open /dev/port: {e}; disabled"));
            return;
        }
    };
    if !std::path::Path::new(GP75_VALUE).exists() {
        let _ = std::fs::write("/sys/class/gpio/export", GP75_GPIO);
    }
    let _ = std::fs::write(GP75_DIR, "out");

    let mut failed = false;
    while let Ok(msg) = rx.recv() {
        let beep = match msg {
            Msg::Beep(beep) => beep,
            Msg::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        if let Err(e) = play(&port, beep.duration()) {
            if !failed {
                syslog::warning(&format!("buzzer: writing port 0x61 failed: {e}"));
            }
            failed = true;
        }
        // A burst of requests would otherwise run together into one tone.
        thread::sleep(Duration::from_millis(100));
    }
}

fn play(port: &File, length: Duration) -> std::io::Result<()> {
    let mut byte = [0u8];
    port.read_exact_at(&mut byte, SPEAKER_PORT)?;
    let idle = byte[0] & !SPEAKER_BITS;

    let _ = std::fs::write(GP75_VALUE, "1");
    let end = Instant::now() + length;
    let mut on = true;
    let mut result = Ok(());
    while Instant::now() < end {
        let value = if on { idle | SPEAKER_BITS } else { idle };
        if let Err(e) = port.write_all_at(&[value], SPEAKER_PORT) {
            result = Err(e);
            break;
        }
        on = !on;
        let next = Instant::now() + HALF_PERIOD;
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
    // Always leave the speaker gate and GP75 as found/idle.
    let restored = port.write_all_at(&[idle], SPEAKER_PORT);
    let _ = std::fs::write(GP75_VALUE, "0");
    result.and(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_beep_is_four_times_the_short_one() {
        assert_eq!(Beep::Long.duration(), Beep::Short.duration() * 4);
    }

    #[test]
    fn disabled_buzzer_ignores_beeps() {
        Buzzer::new(false).beep(Beep::Short);
    }
}
