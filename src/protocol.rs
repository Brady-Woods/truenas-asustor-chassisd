//! Wire protocol for the ASUSTOR front-panel LCM (LCD module), reverse-engineered
//! from ADM 5.1.4.RL21's `lcmd` / `libndal.so`. No ASUSTOR code or libraries used.
//!
//! Frame format: [opcode][N][subcmd][... N payload bytes ...][checksum]
//!   checksum = 8-bit sum of bytes[0 .. N+2], stored at byte[N+3]
//!   wire length = N + 4
//!
//! Serial: /dev/ttyS1, 115200 8N1, opened O_RDWR|O_NOCTTY|O_NONBLOCK, VMIN=1.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

pub const LCM_DEVICE: &str = "/dev/ttyS1";
const FRAME_MAX: usize = 22;

pub struct Lcm {
    port: File,
    /// Unsolicited `0xF0` frames (button presses, version reports) that
    /// `send_and_ack` ran into -- and had to ACK -- while it was waiting
    /// for its own ACK. Drained by `take_pending` so the caller can still
    /// dispatch them instead of losing them.
    pending: Vec<(u8, Vec<u8>)>,
    /// Text last *confirmed* written to each line (index 0/1) -- only set
    /// on a successful ACK, see `set_text`. Lets repeated calls with
    /// unchanged text skip the wire entirely, while a call that failed
    /// keeps retrying on every subsequent call with that text instead of
    /// being silently dropped forever.
    last_sent: [Option<String>; 2],
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub opcode: u8,
    pub subcmd: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    fn raw(&self) -> Vec<u8> {
        let n = self.payload.len().min(18) as u8;
        let mut buf = vec![0u8; n as usize + 4];
        buf[0] = self.opcode;
        buf[1] = n;
        buf[2] = self.subcmd;
        buf[3..3 + n as usize].copy_from_slice(&self.payload[..n as usize]);
        let cksum = checksum(&buf);
        let last = buf.len() - 1;
        buf[last] = cksum;
        buf
    }
}

fn checksum(buf: &[u8]) -> u8 {
    let n = buf[1] as usize + 3;
    buf[..n].iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Parses a raw fixed-size reply buffer into (opcode, subcmd, payload, checksum_ok).
fn parse_reply(buf: &[u8], len: usize) -> Option<(u8, u8, Vec<u8>, bool)> {
    if len < 4 {
        return None;
    }
    let opcode = buf[0];
    let n = buf[1] as usize;
    let subcmd = buf[2];
    let cksum_idx = n + 3;
    let ok = cksum_idx < len && checksum(buf) == buf[cksum_idx];
    let payload_end = (3 + n).min(len);
    let payload = buf[3..payload_end].to_vec();
    Some((opcode, subcmd, payload, ok))
}

impl Lcm {
    pub fn as_raw_fd(&self) -> RawFd {
        self.port.as_raw_fd()
    }

    pub fn open(path: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let port = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)?;

        let fd = port.as_raw_fd();
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            tio.c_cflag = libc::B115200 | libc::CLOCAL as libc::tcflag_t
                | libc::CREAD as libc::tcflag_t
                | libc::CS8 as libc::tcflag_t;
            tio.c_cc[libc::VMIN] = 1;
            tio.c_cc[libc::VTIME] = 0;
            libc::cfsetispeed(&mut tio, libc::B115200);
            libc::cfsetospeed(&mut tio, libc::B115200);
            libc::tcflush(fd, libc::TCIFLUSH);
            if libc::tcsetattr(fd, libc::TCSANOW, &tio) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Lcm { port, pending: Vec::new(), last_sent: [None, None] })
    }

    pub fn send(&mut self, opcode: u8, subcmd: u8, payload: &[u8]) -> io::Result<()> {
        let frame = Frame {
            opcode,
            subcmd,
            payload: payload.to_vec(),
        }
        .raw();
        let n = self.port.write(&frame)?;
        if n != frame.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write to LCM"));
        }
        Ok(())
    }

    /// Reads one fixed-size (up to 22-byte) frame with a timeout, byte at a time,
    /// same approach the original firmware uses. Returns None on timeout.
    pub fn read_frame(&mut self, timeout: Duration) -> io::Result<Option<(u8, u8, Vec<u8>, bool)>> {
        let fd = self.port.as_raw_fd();
        let mut buf = [0u8; FRAME_MAX];
        let mut got = 0usize;
        let start = Instant::now();

        while got < FRAME_MAX && start.elapsed() < timeout {
            let remaining = timeout.saturating_sub(start.elapsed());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe {
                libc::poll(&mut pfd, 1, remaining.as_millis().min(i32::MAX as u128) as i32)
            };
            if rc <= 0 {
                break;
            }
            let mut byte = [0u8; 1];
            match self.port.read(&mut byte) {
                Ok(1) => {
                    buf[got] = byte[0];
                    got += 1;
                    // Byte 1 (N) tells us the exact frame length (N + 4) as
                    // soon as it arrives -- stop there instead of idling
                    // through the rest of `timeout` waiting for bytes that
                    // aren't coming. Without this, every call blocked for
                    // ~the full timeout even on an immediate, complete
                    // reply, which widened the window for an unsolicited
                    // button frame to land mid-`send_and_ack` and get lost.
                    if got >= 2 {
                        let expected = buf[1] as usize + 4;
                        if expected <= FRAME_MAX && got >= expected {
                            break;
                        }
                    }
                }
                _ => continue,
            }
        }

        if got == 0 {
            return Ok(None);
        }
        Ok(parse_reply(&buf, got))
    }

    /// Sends a frame and waits for the corresponding ACK (opcode `0xF1`,
    /// echoing `subcmd`). The MCU can interleave an unsolicited `0xF0`
    /// frame of its own (a button press, a version report) at any time,
    /// including while we're sitting here waiting for our own ACK -- that
    /// frame still has to be ACKed immediately (or the MCU will keep
    /// resending it, see the module doc) rather than silently dropped, so
    /// it's queued in `pending` and reading continues for our actual ACK
    /// within what's left of `timeout`. Call `take_pending` afterward to
    /// pick up anything that got queued this way.
    pub fn send_and_ack(&mut self, opcode: u8, subcmd: u8, payload: &[u8], timeout: Duration) -> io::Result<bool> {
        self.send(opcode, subcmd, payload)?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match self.read_frame(remaining)? {
                Some((op, sc, pl, ok)) if ok && op == 0xF1 && sc == subcmd => {
                    return Ok(pl.first() == Some(&0));
                }
                Some((op, sc, pl, ok)) if ok && op == 0xF0 => {
                    let _ = self.ack(sc);
                    self.pending.push((sc, pl));
                }
                Some(_) => {
                    // Mismatched or checksum-bad frame -- not our ACK,
                    // keep waiting for it.
                }
                None => return Ok(false),
            }
        }
    }

    /// Drains unsolicited MCU frames `send_and_ack` had to queue instead of
    /// discarding (see its doc). Callers should check this after any `Lcm`
    /// call that goes through `send_and_ack` (`set_text`, the init
    /// sequence) so a button press that arrived mid-write isn't missed
    /// until the MCU eventually resends it.
    pub fn take_pending(&mut self) -> Vec<(u8, Vec<u8>)> {
        std::mem::take(&mut self.pending)
    }

    /// Sets up to 16 ASCII chars on the given line (0 or 1), space-padded/truncated.
    /// A no-op (no wire traffic) if `text` is already confirmed showing on
    /// that line -- see `last_sent`.
    pub fn set_text(&mut self, line: u8, text: &str, flag: u8) -> io::Result<bool> {
        let idx = (line & 1) as usize;
        if self.last_sent[idx].as_deref() == Some(text) {
            return Ok(true);
        }

        let mut payload = vec![line, flag];
        let bytes = text.as_bytes();
        let take = bytes.len().min(16);
        payload.extend_from_slice(&bytes[..take]);
        payload.extend(std::iter::repeat(b' ').take(16 - take));

        // The MCU needs a brief settle time after ACKing one command
        // before it'll accept the next -- same reason the power-on init
        // sequence sleeps 15ms between its two steps. Two of these calls
        // fire back-to-back every render (one per line); sent immediately
        // after each other the second one reliably gets NACKed, so retry
        // with that same gap rather than leaving that line stale.
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(15));
            }
            if self.send_and_ack(0xF0, 0x27, &payload, Duration::from_millis(300))? {
                self.last_sent[idx] = Some(text.to_string());
                return Ok(true);
            }
        }
        // Left uncached on failure so the next call with this same text
        // (the caller will keep asking, since as far as it's concerned
        // this is still the text that should be showing) retries instead
        // of being treated as "already sent".
        Ok(false)
    }

    /// Replies to an unsolicited MCU frame the way lcmd does: ACK with status 0.
    pub fn ack(&mut self, subcmd: u8) -> io::Result<()> {
        self.send(0xF1, subcmd, &[0x00])
    }
}

/// Key codes reported by the MCU (subcmd 0x80 unsolicited frames), empirically
/// confirmed against real hardware on 2026-09-22.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Back,
    Enter,
    Wake,
    Unknown(u8),
}

impl From<u8> for Key {
    fn from(code: u8) -> Self {
        match code {
            1 => Key::Up,
            2 => Key::Down,
            3 => Key::Back,
            4 => Key::Enter,
            5 => Key::Wake,
            other => Key::Unknown(other),
        }
    }
}
