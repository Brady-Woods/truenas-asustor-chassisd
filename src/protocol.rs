//! Wire protocol for the ASUSTOR front-panel LCM (LCD module), reverse-engineered
//! from ADM 5.1.4.RL21's `lcmd` / `libndal.so`. No ASUSTOR code or libraries used.
//!
//! Frame format: [opcode][N][subcmd][... N payload bytes ...][checksum]
//!   checksum = 8-bit sum of bytes[0 .. N+2], stored at byte[N+3]
//!   wire length = N + 4
//!
//! Serial: /dev/ttyS1, 115200 8N1, opened `O_RDWR|O_NOCTTY|O_NONBLOCK`, VMIN=1.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

pub const LCM_DEVICE: &str = "/dev/ttyS1";

/// Opcode of a frame that initiates something: a host command, or an
/// unsolicited MCU report (key press, version).
pub const OP_COMMAND: u8 = 0xF0;
/// Opcode of an acknowledgement; its subcmd echoes the frame it answers.
pub const OP_ACK: u8 = 0xF1;
/// MCU report: a front-panel key press, `payload[0]` = key code.
pub const SUB_KEY: u8 = 0x80;
/// MCU report: firmware version, `payload` = major, minor, patch.
pub const SUB_VERSION: u8 = 0x13;
/// Host command: write one 16-char line of text.
const SUB_SET_TEXT: u8 = 0x27;
/// Host command: display on (`[0x01]`) or off (`[0x00]`). Also step 1 of
/// the stock firmware's power-on sequence (with 1). "Off" darkens the
/// whole display, backlight included, while the MCU stays powered: it
/// still reports key presses, and turns the display back on by itself
/// when one comes in (confirmed live 2026-10-02). ADM's `lcmd` uses it the
/// same way for its own idle timeout -- unlike cutting `power:lcd`, which
/// also silences the buttons.
const SUB_DISPLAY: u8 = 0x11;
/// Step 2 of the power-on sequence. `lcmd` also sends it leaving its
/// menu, so probably "edit cursor off".
const SUB_INIT_2: u8 = 0x22;

/// Longest frame on the wire: header (3) + payload + checksum (1).
const FRAME_MAX: usize = 22;
const PAYLOAD_MAX: usize = FRAME_MAX - 4;
/// Characters per display line.
const LINE_WIDTH: usize = 16;
/// How long to wait for the MCU to ACK a command.
const ACK_TIMEOUT: Duration = Duration::from_millis(300);
/// Quiet time the MCU needs after sending a frame (an ACK, or a key press)
/// before it reliably receives the next one. Measured live (2026-10-02,
/// a wire trace): a frame sent within ~0.1ms of the MCU's ACK for the
/// previous one -- line 1 straight after line 0 -- went entirely
/// unanswered about 1 time in 7, presumably while the MCU was busy
/// updating the LCD. Worse, when only *part* of a frame was dropped, the
/// leftover bytes merged with the next frame and put stray text on the
/// panel (e.g. the end of one line's IP address showing on the other).
/// With a 10ms or 20ms gap after every received frame: 240 writes, zero
/// failures. 20ms keeps 2x margin. Applied in `send`, so it covers every
/// write, and is also the retry backoff in `set_text`.
const SETTLE: Duration = Duration::from_millis(20);

pub struct Lcm {
    port: File,
    /// Unsolicited frames (button presses, version reports) that
    /// `send_and_ack` ran into -- and had to ACK -- while it was waiting
    /// for its own ACK. Drained by `take_pending` so the caller can still
    /// dispatch them instead of losing them.
    pending: Vec<Frame>,
    /// Text last *confirmed* written to each line (index 0/1) -- only set
    /// on a successful ACK, see `set_text`. Lets repeated calls with
    /// unchanged text skip the wire entirely, while a call that failed
    /// keeps retrying on every subsequent call with that text instead of
    /// being silently dropped forever.
    last_sent: [Option<String>; 2],
    /// Display state last *confirmed* by an ACK (`SUB_DISPLAY`), same
    /// idea as `last_sent`. Can go stale the other way: the MCU switches
    /// the display on by itself on a key press -- harmless, since the
    /// caller wakes on that same key press and asks for on anyway.
    display_on: Option<bool>,
    /// When the last byte arrived from the MCU -- `send` waits out
    /// `SETTLE` from here.
    last_rx: Option<Instant>,
    /// Running totals for the `status` report -- see `LinkStats`.
    stats: LinkStats,
    /// Wire trace, when `LCM_STATUS_TRACE` names a file: every byte sent
    /// and received, timestamped. For debugging the panel link (this is
    /// how `SETTLE` was found); off by default.
    trace: Option<(File, Instant)>,
}

/// How text writes to the panel have gone since the daemon started. A
/// retry means the MCU didn't ACK a write the first time; a failure means
/// it never did (the line is retried on the next render). Both should
/// stay at or near zero -- see `SETTLE`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStats {
    pub writes: u64,
    pub retries: u64,
    pub failures: u64,
}

/// One received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub opcode: u8,
    pub subcmd: u8,
    pub payload: Vec<u8>,
    pub checksum_ok: bool,
}

impl Frame {
    /// True for a valid unsolicited MCU frame, which must be ACKed.
    pub fn is_unsolicited(&self) -> bool {
        self.checksum_ok && self.opcode == OP_COMMAND
    }

    /// The key, if this is a valid key-press report.
    pub fn key(&self) -> Option<Key> {
        (self.is_unsolicited() && self.subcmd == SUB_KEY)
            .then(|| self.payload.first().copied().map(Key::from))
            .flatten()
    }
}

/// Serializes a frame; payloads beyond the protocol maximum are truncated.
fn encode(opcode: u8, subcmd: u8, payload: &[u8]) -> Vec<u8> {
    let payload = &payload[..payload.len().min(PAYLOAD_MAX)];
    let mut buf = Vec::with_capacity(payload.len() + 4);
    buf.push(opcode);
    buf.push(u8::try_from(payload.len()).expect("payload is at most PAYLOAD_MAX bytes"));
    buf.push(subcmd);
    buf.extend_from_slice(payload);
    buf.push(checksum(&buf));
    buf
}

/// 8-bit sum of a frame's header and payload.
fn checksum(header_and_payload: &[u8]) -> u8 {
    header_and_payload
        .iter()
        .fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Parses the bytes of one received frame. `None` if too short to even
/// have a header; a frame that's truncated or fails its checksum is still
/// returned, with `checksum_ok` false.
fn decode(buf: &[u8]) -> Option<Frame> {
    if buf.len() < 4 {
        return None;
    }
    let n = usize::from(buf[1]);
    let cksum_idx = n + 3;
    let checksum_ok = cksum_idx < buf.len() && checksum(&buf[..cksum_idx]) == buf[cksum_idx];
    Some(Frame {
        opcode: buf[0],
        subcmd: buf[2],
        payload: buf[3..(3 + n).min(buf.len())].to_vec(),
        checksum_ok,
    })
}

impl AsRawFd for Lcm {
    fn as_raw_fd(&self) -> RawFd {
        self.port.as_raw_fd()
    }
}

impl Lcm {
    pub fn open(path: &str) -> io::Result<Self> {
        let port = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)?;

        let fd = port.as_raw_fd();
        // SAFETY: `termios` is plain data for which all-zeroes is valid,
        // every pointer passed is to that local, and `fd` stays open for
        // the duration (owned by `port`).
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            tio.c_cflag = libc::B115200 | libc::CLOCAL | libc::CREAD | libc::CS8;
            tio.c_cc[libc::VMIN] = 1;
            tio.c_cc[libc::VTIME] = 0;
            libc::cfsetispeed(&raw mut tio, libc::B115200);
            libc::cfsetospeed(&raw mut tio, libc::B115200);
            libc::tcflush(fd, libc::TCIFLUSH);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw const tio) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Lcm {
            port,
            pending: Vec::new(),
            last_sent: [None, None],
            display_on: None,
            last_rx: None,
            stats: LinkStats::default(),
            trace: std::env::var_os("LCM_STATUS_TRACE").and_then(|path| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
                    .map(|f| (f, Instant::now()))
            }),
        })
    }

    /// Sends the stock firmware's power-on sequence. Returns whether each
    /// of its two steps was ACKed.
    pub fn init(&mut self) -> io::Result<(bool, bool)> {
        let first = self.send_and_ack(OP_COMMAND, SUB_DISPLAY, &[0x01], ACK_TIMEOUT)?;
        self.display_on = first.then_some(true);
        let second = self.send_and_ack(OP_COMMAND, SUB_INIT_2, &[0x00], ACK_TIMEOUT)?;
        Ok((first, second))
    }

    /// Writes one frame, first waiting out `SETTLE` since the MCU last
    /// sent anything.
    pub fn send(&mut self, opcode: u8, subcmd: u8, payload: &[u8]) -> io::Result<()> {
        if let Some(last) = self.last_rx {
            let wait = SETTLE.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        let wire = encode(opcode, subcmd, payload);
        self.trace_bytes("TX", &wire);
        self.port.write_all(&wire)
    }

    /// Reads one frame (up to `FRAME_MAX` bytes) with a timeout, byte at a
    /// time, the same approach the original firmware uses. `None` on
    /// timeout (or a read error) with nothing received.
    pub fn read_frame(&mut self, timeout: Duration) -> Option<Frame> {
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
            let timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: `pfd` is a valid pollfd for the duration of the call,
            // and the count (1) matches.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
            if rc <= 0 {
                break;
            }
            let mut byte = [0u8; 1];
            match self.port.read(&mut byte) {
                Ok(1) => {
                    self.last_rx = Some(Instant::now());
                    self.trace_bytes("RX", &byte);
                    // Resync: a frame only starts at an opcode byte. Anything
                    // else ahead of one is the tail of a frame we lost the
                    // start of; taking it as a header would misread the
                    // length and swallow the next real frame with it.
                    if got == 0 && !matches!(byte[0], OP_COMMAND | OP_ACK) {
                        continue;
                    }
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
                        let expected = usize::from(buf[1]) + 4;
                        if expected <= FRAME_MAX && got >= expected {
                            break;
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                // EOF or a real error: poll() would keep reporting the fd
                // ready, so retrying would just spin until the timeout.
                _ => break,
            }
        }

        if got == 0 {
            return None;
        }
        decode(&buf[..got])
    }

    /// Sends a frame and waits for the corresponding ACK (`OP_ACK`,
    /// echoing `subcmd`). The MCU can interleave an unsolicited frame of
    /// its own (a button press, a version report) at any time, including
    /// while we're sitting here waiting for our own ACK -- that frame
    /// still has to be ACKed immediately (or the MCU will keep resending
    /// it) rather than silently dropped, so it's queued in `pending` and
    /// reading continues for our actual ACK within what's left of
    /// `timeout`. Call `take_pending` afterward to pick up anything that
    /// got queued this way.
    pub fn send_and_ack(
        &mut self,
        opcode: u8,
        subcmd: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> io::Result<bool> {
        self.send(opcode, subcmd, payload)?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match self.read_frame(remaining) {
                Some(f) if f.checksum_ok && f.opcode == OP_ACK && f.subcmd == subcmd => {
                    return Ok(f.payload.first() == Some(&0));
                }
                Some(f) if f.is_unsolicited() => {
                    let _ = self.ack(f.subcmd);
                    self.pending.push(f);
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
    /// call that goes through `send_and_ack` (`set_text`, `init`) so a
    /// button press that arrived mid-write isn't missed until the MCU
    /// eventually resends it.
    pub fn take_pending(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.pending)
    }

    /// Sets the given line (0 or 1) to `text`, space-padded or truncated to
    /// 16 characters. The panel is ASCII-only; anything else is shown as
    /// `?`. A no-op (no wire traffic) if `text` is already confirmed
    /// showing on that line -- see `last_sent`.
    pub fn set_text(&mut self, line: u8, text: &str, flag: u8) -> io::Result<bool> {
        let idx = usize::from(line & 1);
        if self.last_sent[idx].as_deref() == Some(text) {
            return Ok(true);
        }

        let mut payload = vec![line, flag];
        payload.extend(line_bytes(text));

        // `send` paces every write (see `SETTLE`), so a retry should be
        // rare; it's still kept, with the same backoff, rather than leaving
        // a line stale.
        self.stats.writes += 1;
        for attempt in 0..3 {
            if attempt > 0 {
                self.stats.retries += 1;
                std::thread::sleep(SETTLE);
            }
            self.trace_note(&format!("text line{line} {text:?} attempt{attempt}"));
            if self.send_and_ack(OP_COMMAND, SUB_SET_TEXT, &payload, ACK_TIMEOUT)? {
                self.last_sent[idx] = Some(text.to_string());
                return Ok(true);
            }
            self.trace_note("  no ACK (or NAK)");
        }
        self.stats.failures += 1;
        // Left uncached on failure so the next call with this same text
        // (the caller will keep asking, since as far as it's concerned
        // this is still the text that should be showing) retries instead
        // of being treated as "already sent".
        Ok(false)
    }

    fn trace_bytes(&mut self, dir: &str, bytes: &[u8]) {
        if self.trace.is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            self.trace_note(&format!("{dir} {}", hex.join(" ")));
        }
    }

    fn trace_note(&mut self, note: &str) {
        if let Some((file, start)) = &mut self.trace {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let _ = writeln!(file, "{ms:10.1} {note}");
        }
    }

    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// Forgets what the panel is confirmed to be showing, so the next
    /// `set_text`/`set_display` goes out on the wire even if unchanged.
    /// The caches assume the panel still shows what it last ACKed, which
    /// stops being true if its MCU resets (it then shows its own boot text
    /// until told otherwise) -- the caller does this when the MCU reports
    /// its version (which it does on boot) and periodically, so any
    /// mismatch heals within one redraw period.
    pub fn forget_shown(&mut self) {
        self.last_sent = [None, None];
        self.display_on = None;
    }

    /// Switches the display (backlight included) on or off -- see
    /// `SUB_DISPLAY`. A no-op if that state is already confirmed.
    pub fn set_display(&mut self, on: bool) -> io::Result<bool> {
        if self.display_on == Some(on) {
            return Ok(true);
        }
        let acked = self.send_and_ack(OP_COMMAND, SUB_DISPLAY, &[u8::from(on)], ACK_TIMEOUT)?;
        if acked {
            self.display_on = Some(on);
        }
        Ok(acked)
    }

    /// Replies to an unsolicited MCU frame the way lcmd does: ACK with status 0.
    pub fn ack(&mut self, subcmd: u8) -> io::Result<()> {
        self.send(OP_ACK, subcmd, &[0x00])
    }
}

/// Exactly `LINE_WIDTH` printable-ASCII bytes for one display line.
fn line_bytes(text: &str) -> impl Iterator<Item = u8> + '_ {
    text.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c as u8
            } else {
                b'?'
            }
        })
        .chain(std::iter::repeat(b' '))
        .take(LINE_WIDTH)
}

/// Key codes reported by the MCU (`SUB_KEY` unsolicited frames), empirically
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// An `Lcm` on the slave side of a fresh pty, plus the master side
    /// standing in for the panel's MCU.
    fn lcm_on_pty() -> (Lcm, File) {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes the two fds it creates into the locals
        // passed; the optional name/termios/winsize pointers may be null.
        let rc = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: `slave` is a valid open fd; ttyname returns a pointer to
        // a static NUL-terminated buffer, copied out before any other call.
        let path = unsafe { std::ffi::CStr::from_ptr(libc::ttyname(slave)) }
            .to_string_lossy()
            .into_owned();
        let lcm = Lcm::open(&path).unwrap();
        // SAFETY: both fds are open and owned by nothing else from here.
        unsafe {
            libc::close(slave);
            (lcm, File::from_raw_fd(master))
        }
    }

    #[test]
    fn read_frame_skips_bytes_ahead_of_a_frame_start() {
        let (mut lcm, mut mcu) = lcm_on_pty();
        // The tail of a frame whose start was lost, then a real key press.
        let mut wire = vec![0x27, 0x04, 0x1d];
        wire.extend(encode(OP_COMMAND, SUB_KEY, &[2]));
        mcu.write_all(&wire).unwrap();
        let f = lcm.read_frame(Duration::from_millis(500)).unwrap();
        assert!(f.checksum_ok);
        assert_eq!(f.key(), Some(Key::Down));
    }

    #[test]
    fn send_waits_out_settle_after_the_mcu_last_spoke() {
        let (mut lcm, mut mcu) = lcm_on_pty();
        mcu.write_all(&encode(OP_ACK, SUB_SET_TEXT, &[0])).unwrap();
        lcm.read_frame(Duration::from_millis(500)).unwrap();
        let start = Instant::now();
        lcm.send(OP_COMMAND, SUB_DISPLAY, &[1]).unwrap();
        assert!(start.elapsed() >= SETTLE.saturating_sub(Duration::from_millis(1)));
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let wire = encode(OP_COMMAND, SUB_SET_TEXT, &[0, 0, b'H', b'I']);
        assert_eq!(wire.len(), 4 + 4);
        assert_eq!(
            decode(&wire),
            Some(Frame {
                opcode: OP_COMMAND,
                subcmd: SUB_SET_TEXT,
                payload: vec![0, 0, b'H', b'I'],
                checksum_ok: true,
            })
        );
    }

    #[test]
    fn checksum_matches_a_captured_ack() {
        // F1 01 27 00 -> 0xF1 + 0x01 + 0x27 + 0x00 = 0x119 -> 0x19
        assert_eq!(
            encode(OP_ACK, SUB_SET_TEXT, &[0]),
            vec![0xF1, 0x01, 0x27, 0x00, 0x19]
        );
    }

    #[test]
    fn corrupt_or_truncated_frames_fail_the_checksum() {
        let mut wire = encode(OP_COMMAND, SUB_KEY, &[1]);
        *wire.last_mut().unwrap() ^= 0xFF;
        assert!(!decode(&wire).unwrap().checksum_ok);

        let wire = encode(OP_COMMAND, SUB_KEY, &[1]);
        assert!(!decode(&wire[..wire.len() - 1]).unwrap().checksum_ok);
        assert_eq!(decode(&wire[..3]), None);
    }

    #[test]
    fn key_is_only_reported_for_valid_key_frames() {
        let key = decode(&encode(OP_COMMAND, SUB_KEY, &[4])).unwrap();
        assert_eq!(key.key(), Some(Key::Enter));
        let version = decode(&encode(OP_COMMAND, SUB_VERSION, &[1, 2, 3])).unwrap();
        assert_eq!(version.key(), None);
        let ack = decode(&encode(OP_ACK, SUB_KEY, &[4])).unwrap();
        assert_eq!(ack.key(), None);
    }

    #[test]
    fn oversized_payload_is_truncated_to_the_protocol_maximum() {
        let wire = encode(OP_COMMAND, SUB_SET_TEXT, &[b'x'; 40]);
        assert_eq!(wire.len(), FRAME_MAX);
        assert!(decode(&wire).unwrap().checksum_ok);
    }

    #[test]
    fn line_bytes_pads_truncates_and_replaces_non_ascii() {
        let pad: Vec<u8> = line_bytes("OK").collect();
        assert_eq!(pad, b"OK              ");
        let long: Vec<u8> = line_bytes("0123456789abcdefXYZ").collect();
        assert_eq!(long, b"0123456789abcdef");
        let utf8: Vec<u8> = line_bytes("40\u{b0}C").collect();
        assert_eq!(utf8, b"40?C            ");
    }
}
