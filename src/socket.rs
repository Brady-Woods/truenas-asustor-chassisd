//! Unix socket listener: lets other processes (an LED daemon, cron jobs,
//! ad-hoc scripts) push text onto the display, blink a drive bay's LEDs
//! to find it (`LOCATE`), or (`STATUS`) pull a terminal-readable health
//! report back out. Runs on its own thread and
//! hands parsed commands to the main event loop over a bounded channel,
//! so the socket's blocking `accept()` loop never touches the
//! display/fan state directly.
//!
//! Anyone in the socket's group can connect, so everything a client can
//! make the daemon do is bounded: at most `MAX_CONNECTIONS` connections
//! at once (the rest are told `ERR busy` and closed), each given
//! `REQUEST_DEADLINE` in total to send its request however slowly it
//! drips it, and a command queue of `COMMAND_QUEUE_DEPTH` that drops
//! (and says `ERR busy`) rather than grows when the main loop can't keep
//! up. The main loop drains at most `MAX_COMMANDS_PER_PASS` per
//! iteration (`drain`), so a flood can never starve the fans or LCD.
//!
//! `STATUS` is the one request/response case: everything else here is
//! fire-and-forget (the sender doesn't wait for a reply), but a status
//! report needs live data only the main loop has (fan stall history,
//! active overrides) -- so this asks for one, using a fresh one-shot
//! `mpsc` channel per request as the reply path, and waits (up to a
//! timeout) for the main loop to compute and send one back before writing
//! it to the client and closing the connection.
//!
//! A valid fire-and-forget request gets no reply at all (existing
//! clients just close). A malformed one gets a single `ERR <reason>\n`
//! line, written best-effort: a client that never reads it is unaffected.

use crate::syslog;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

/// Ordered so `a < b` means "b is at least as severe" -- used both for the
/// LCD override precedence (a higher level can replace a lower one, never
/// the reverse) and for the status LED color/pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Error,
    Critical,
}

impl Level {
    /// `None` for anything that isn't a level: a typo'd `crtical` must not
    /// quietly become an `info` that never raises the alarm it was meant to.
    fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            "critical" => Some(Level::Critical),
            _ => None,
        }
    }

    /// Error and critical are meant to always be seen: they persist on the
    /// LCD (rather than expiring on a TTL) and wake the panel from sleep.
    pub fn always_visible(self) -> bool {
        self >= Level::Error
    }
}

#[derive(Clone)]
pub enum SocketCommand {
    Show {
        level: Level,
        ttl_secs: u64,
        /// Optional bay this alert is about (e.g. from `SHOW error 0
        /// bay=2`). error/critical with a bay set also flashes that bay's
        /// red LED (a distinct "alert" pattern from a confirmed SMART
        /// failure), not just the general status LED.
        bay: Option<u32>,
        line0: String,
        line1: String,
    },
    Clear {
        bay: Option<u32>,
    },
    /// `LOCATE [bay=N] [ttl_secs]`: blink a bay's LEDs (or, with no bay,
    /// the whole chassis's: power + status LEDs, hostname on the LCD) so
    /// someone standing at the rack can find it -- ADM's "inspection LED".
    /// Lasts `ttl_secs` (default `DEFAULT_LOCATE_TTL_SECS`; `0` = until
    /// `LOCATE off`). Independent of `SHOW`/`CLEAR`: a bare `CLEAR` from
    /// some script tidying up its own alert doesn't cut a person's locate
    /// short. Repeating it for something already blinking restarts its TTL.
    Locate {
        bay: Option<u32>,
        ttl_secs: u64,
    },
    /// `LOCATE off [bay=N]`: stop locating that bay, or with no bay, stop
    /// every active locate (chassis and all bays) -- the "make it stop"
    /// button doesn't need to know what was started.
    LocateOff {
        bay: Option<u32>,
    },
    /// A `STATUS` request came in on some connection; send the current
    /// terminal-readable report (see `report.rs`) back on this channel.
    /// One fresh channel per request, not a `Debug`/`PartialEq`-friendly
    /// payload like the other variants, hence the manual `Clone`-only
    /// derive above (`mpsc::Sender` is `Clone` but not `Debug`).
    StatusRequest(Sender<String>),
}

/// How long a `LOCATE` that doesn't give a TTL keeps blinking: long enough
/// to walk to the rack, short enough that a forgotten one stops by itself.
pub const DEFAULT_LOCATE_TTL_SECS: u64 = 60;

/// What a connection asked for.
enum Request {
    Status,
    Command(SocketCommand),
}

/// Parses a whole request: `STATUS`, or a message for `parse_message`.
/// `Err` is the reason sent back to the client as `ERR <reason>`.
fn parse_request(lines: &[String]) -> Result<Request, String> {
    let first = lines.first().map_or("", |l| l.trim());
    let mut words = first.split_whitespace();
    if words
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("STATUS"))
    {
        // Not bare text: `STATUS now` showing up on the LCD helps nobody.
        return if words.next().is_none() {
            Ok(Request::Status)
        } else {
            Err("STATUS takes no arguments".into())
        };
    }
    parse_message(lines).map(Request::Command)
}

/// Parses one message from a connection: either a `SHOW`/`CLEAR`/`LOCATE`
/// header followed by up to two content lines, or bare text (shorthand for
/// `SHOW info 5 <line0>\n<line1>`). Header line: `SHOW <level> <ttl>
/// [bay=N]` / `CLEAR [bay=N]` / `LOCATE [off] [bay=N] [ttl]`. A header
/// that is recognized but malformed is an error (never guessed at, or
/// widened to something bigger: a typo'd `CLEAR bay=` must not clear
/// every alert).
fn parse_message(lines: &[String]) -> Result<SocketCommand, String> {
    let first = lines.first().map_or("", |l| l.trim());
    let mut parts = first.split_whitespace();
    let Some(verb) = parts.next() else {
        return Err("empty request".into());
    };

    match verb.to_ascii_uppercase().as_str() {
        "CLEAR" => parse_bay(parts).map(|bay| SocketCommand::Clear { bay }),
        "LOCATE" => parse_locate(parts),
        "SHOW" => {
            const USAGE: &str = "SHOW needs <level> <ttl_secs> [bay=N]";
            let level = parts.next().ok_or(USAGE)?;
            let level = Level::parse(level).ok_or_else(|| {
                format!(
                    "unknown level {}; expected info, warn, error or critical",
                    quote(level)
                )
            })?;
            let ttl = parts.next().ok_or(USAGE)?;
            let ttl_secs = ttl
                .parse()
                .map_err(|_| format!("bad ttl_secs {}; expected whole seconds", quote(ttl)))?;
            let bay = parse_bay(parts)?;
            Ok(SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0: lines.get(1).cloned().unwrap_or_default(),
                line1: lines.get(2).cloned().unwrap_or_default(),
            })
        }
        _ => {
            // Bare text shorthand: first line -> line0, second -> line1.
            Ok(SocketCommand::Show {
                level: Level::Info,
                ttl_secs: 5,
                bay: None,
                line0: lines.first().cloned().unwrap_or_default(),
                line1: lines.get(1).cloned().unwrap_or_default(),
            })
        }
    }
}

/// `s` as a short quoted, escaped string, safe to echo in a one-line
/// `ERR` reply whatever bytes the client sent.
fn quote(s: &str) -> String {
    format!("{:?}", s.chars().take(24).collect::<String>())
}

/// The `N` of a `bay=N` argument; bays count from 1.
fn bay_number(n: &str) -> Result<u32, String> {
    n.parse()
        .ok()
        .filter(|&b| b > 0)
        .ok_or_else(|| format!("bad bay {}; bays are numbered from 1", quote(n)))
}

/// The optional `bay=N` after `SHOW`/`CLEAR`. Anything else, or a second
/// `bay=`, is an error: a typo'd `bay=` rejects the request rather than
/// silently becoming "no bay" (which `CLEAR` would read as "all of them").
fn parse_bay<'a>(parts: impl Iterator<Item = &'a str>) -> Result<Option<u32>, String> {
    let mut bay = None;
    for part in parts {
        let Some(n) = part.strip_prefix("bay=") else {
            return Err(format!("unexpected argument {}", quote(part)));
        };
        if bay.replace(bay_number(n)?).is_some() {
            return Err("bay given twice".into());
        }
    }
    Ok(bay)
}

/// The arguments after `LOCATE`, in any order: `off`, `bay=N`, and a TTL
/// in seconds. Strict for the same reason as `parse_bay`: a typo'd
/// `bay=` (or `bay=0` -- bays count from 1) rejects the whole request
/// rather than silently falling back to locating the entire chassis.
fn parse_locate<'a>(parts: impl Iterator<Item = &'a str>) -> Result<SocketCommand, String> {
    let (mut off, mut bay, mut ttl_secs) = (false, None, None);
    for part in parts {
        if part.eq_ignore_ascii_case("off") {
            off = true;
        } else if let Some(n) = part.strip_prefix("bay=") {
            if bay.replace(bay_number(n)?).is_some() {
                return Err("bay given twice".into());
            }
        } else {
            let secs = part.parse().map_err(|_| {
                format!(
                    "unexpected argument {}; expected off, bay=N or a ttl in seconds",
                    quote(part)
                )
            })?;
            if ttl_secs.replace(secs).is_some() {
                return Err("ttl given twice".into());
            }
        }
    }
    if off {
        // A TTL on a request to stop makes no sense; refuse rather than guess.
        if ttl_secs.is_some() {
            return Err("LOCATE off takes no ttl".into());
        }
        Ok(SocketCommand::LocateOff { bay })
    } else {
        Ok(SocketCommand::Locate {
            bay,
            ttl_secs: ttl_secs.unwrap_or(DEFAULT_LOCATE_TTL_SECS),
        })
    }
}

/// The `LOCATE` header line `parse_locate` accepts, for the client side
/// (`lcm-status locate`). `ttl_secs: None` leaves the TTL to the daemon.
pub fn locate_request(bay: Option<u32>, ttl_secs: Option<u64>, off: bool) -> String {
    let mut parts = vec!["LOCATE".to_string()];
    if off {
        parts.push("off".to_string());
    }
    parts.extend(bay.map(|b| format!("bay={b}")));
    parts.extend(ttl_secs.map(|t| t.to_string()));
    parts.join(" ")
}

/// Commands waiting for the main loop. A full queue drops new requests
/// (`ERR busy`) instead of growing without bound.
const COMMAND_QUEUE_DEPTH: usize = 64;

/// The most commands the main loop takes per iteration, so the fan, LCD
/// and shutdown checks that share the loop always get their turn.
const MAX_COMMANDS_PER_PASS: usize = 16;

/// Connections served at once; further ones are turned away.
const MAX_CONNECTIONS: usize = 16;

/// How long a client may sit idle (send nothing, or accept nothing of a
/// reply) before the connection is dropped.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a client has, in total, to deliver its request. The idle
/// timeout alone would let a client that drips one byte every
/// second hold a connection (and a thread) for as long as it likes.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// How long writing the reply may take in total, for the same reason.
const REPLY_DEADLINE: Duration = Duration::from_secs(5);

/// How long a `STATUS` request waits for the main loop to build the
/// report. Generous because the main loop answers between its own
/// passes, and a pass can be inside a refresh that runs `zpool`,
/// `smartctl` or `docker` (each of which `hal` allows up to 10s before
/// giving up); the report itself only needs cached state, sysfs and `ip`.
const STATUS_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause after a failed `accept()` (out of file descriptors, say) or a
/// failed thread spawn, so a persistent failure doesn't spin the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Requests are a header plus two short lines; anything past this is
/// ignored.
const MAX_REQUEST_BYTES: u64 = 4096;

/// The time limits and caps a listener runs with; tests shrink them.
#[derive(Debug, Clone, Copy)]
struct Limits {
    max_connections: usize,
    io_timeout: Duration,
    request_deadline: Duration,
    reply_deadline: Duration,
    accept_backoff: Duration,
}

impl Limits {
    const DEFAULT: Limits = Limits {
        max_connections: MAX_CONNECTIONS,
        io_timeout: CLIENT_IO_TIMEOUT,
        request_deadline: REQUEST_DEADLINE,
        reply_deadline: REPLY_DEADLINE,
        accept_backoff: ACCEPT_BACKOFF,
    };
}

/// A command queue and its receiving end for the main loop.
pub fn command_channel() -> (SyncSender<SocketCommand>, Receiver<SocketCommand>) {
    mpsc::sync_channel(COMMAND_QUEUE_DEPTH)
}

/// The commands the main loop should handle this iteration: at most
/// `MAX_COMMANDS_PER_PASS`, the rest stay queued for the next one.
pub fn drain(rx: &Receiver<SocketCommand>) -> impl Iterator<Item = SocketCommand> {
    rx.try_iter().take(MAX_COMMANDS_PER_PASS)
}

/// A `Read`/`Write` view of a stream that gives up once `deadline` has
/// passed, however steadily bytes keep trickling through: each call is
/// allowed the idle timeout or what's left of the deadline, whichever is
/// shorter.
struct Deadline<'a> {
    stream: &'a UnixStream,
    until: Instant,
    idle: Duration,
}

impl Deadline<'_> {
    fn budget(&self) -> io::Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(left.min(self.idle))
        }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.budget()?))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.budget()?))?;
        let mut stream = self.stream;
        stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The request bytes: everything up to EOF, the size cap, the idle
/// timeout or the deadline. When it ended any way but a clean EOF the
/// last line may be cut short, so an unterminated tail is dropped rather
/// than acted on; the same when the size cap cut a request off.
fn read_request(stream: &UnixStream, limits: Limits) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut reader = Deadline {
        stream,
        until: Instant::now() + limits.request_deadline,
        idle: limits.io_timeout,
    }
    .take(MAX_REQUEST_BYTES);
    let clean = reader.read_to_end(&mut buf).is_ok();
    let capped = u64::try_from(buf.len()).is_ok_and(|n| n >= MAX_REQUEST_BYTES);
    if !clean || capped {
        match buf.iter().rposition(|&b| b == b'\n') {
            Some(last) => buf.truncate(last + 1),
            None if !clean => buf.clear(),
            None => {}
        }
    }
    buf
}

/// Queues `cmd` for the main loop without ever waiting for room.
fn enqueue(tx: &SyncSender<SocketCommand>, cmd: SocketCommand) -> Result<(), String> {
    tx.try_send(cmd).map_err(|e| match e {
        TrySendError::Full(_) => "busy: too many commands queued".to_string(),
        TrySendError::Disconnected(_) => "daemon is shutting down".to_string(),
    })
}

/// Asks the main loop for the `STATUS` report and waits for it.
fn status_report(tx: &SyncSender<SocketCommand>) -> Result<String, String> {
    let (resp_tx, resp_rx) = mpsc::channel();
    enqueue(tx, SocketCommand::StatusRequest(resp_tx))?;
    // Normally resolves within one ~100ms main-loop pass; the timeout
    // is so a client can't hang forever if the main loop is wedged.
    resp_rx
        .recv_timeout(STATUS_REPLY_TIMEOUT)
        .map_err(|_| "timed out waiting for the status report".to_string())
}

/// Acts on one request. `Ok(Some(text))` is a reply to send, `Ok(None)`
/// a fire-and-forget request that was queued, `Err` the reason it wasn't.
fn process(raw: &[u8], tx: &SyncSender<SocketCommand>) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "request is not valid UTF-8".to_string())?;
    let lines: Vec<String> = text.lines().map(String::from).collect();
    match parse_request(&lines)? {
        Request::Status => status_report(tx).map(Some),
        Request::Command(cmd) => enqueue(tx, cmd).map(|()| None),
    }
}

fn handle_connection(stream: &UnixStream, tx: &SyncSender<SocketCommand>, limits: Limits) {
    let raw = read_request(stream, limits);
    if raw.is_empty() {
        return;
    }
    let reply = match process(&raw, tx) {
        Ok(Some(reply)) => reply,
        Ok(None) => return,
        Err(reason) => format!("ERR {reason}\n"),
    };
    let mut writer = Deadline {
        stream,
        until: Instant::now() + limits.reply_deadline,
        idle: limits.io_timeout,
    };
    // Best effort: a client that has gone, or won't read, loses the reply.
    let _ = writer.write_all(reply.as_bytes());
}

/// One of the `max_connections` places; freed when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn acquire(active: &Arc<AtomicUsize>, max: usize) -> Option<Slot> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Tells a connection that was turned away why, without waiting on it.
fn reject(stream: &UnixStream, reply: &str) {
    if stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .is_ok()
    {
        let mut stream = stream;
        let _ = stream.write_all(reply.as_bytes());
    }
}

/// Serves `incoming` connections, one thread each, up to the cap. A
/// failing `accept()` is logged (once per run of failures) and backed off
/// from, never spun on. Returns when `incoming` ends, which for a real
/// listener is never.
fn serve(
    incoming: impl Iterator<Item = io::Result<UnixStream>>,
    tx: &SyncSender<SocketCommand>,
    limits: Limits,
) {
    let active = Arc::new(AtomicUsize::new(0));
    let (mut accept_failing, mut spawn_failing) = (false, false);
    for conn in incoming {
        let stream = match conn {
            Ok(stream) => {
                accept_failing = false;
                stream
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue;
            }
            Err(e) => {
                if !accept_failing {
                    syslog::warning(&format!("socket: accept failed: {e}; retrying"));
                    accept_failing = true;
                }
                thread::sleep(limits.accept_backoff);
                continue;
            }
        };
        let Some(slot) = Slot::acquire(&active, limits.max_connections) else {
            reject(&stream, "ERR busy: too many connections\n");
            continue;
        };
        let tx = tx.clone();
        let spawned = thread::Builder::new()
            .name("socket-conn".into())
            .spawn(move || {
                let _slot = slot;
                handle_connection(&stream, &tx, limits);
            });
        match spawned {
            Ok(_) => spawn_failing = false,
            Err(e) => {
                // The closure, and with it the connection and its slot, is
                // dropped: the client sees the connection close.
                if !spawn_failing {
                    syslog::warning(&format!("socket: could not start a connection thread: {e}"));
                    spawn_failing = true;
                }
                thread::sleep(limits.accept_backoff);
            }
        }
    }
}

/// Removes the socket file when dropped (clean shutdown), but only if it
/// is still the one this daemon created: a replacement put there since is
/// left alone.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

impl SocketGuard {
    fn new(path: &Path) -> SocketGuard {
        SocketGuard {
            path: path.to_path_buf(),
            identity: file_identity(path),
        }
    }
}

/// Device and inode of what's at `path`, without following a symlink.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self.identity.is_none() || file_identity(&self.path) != self.identity {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => syslog::warning(&format!(
                "socket: could not remove {}: {e}",
                self.path.display()
            )),
        }
    }
}

/// Starts the socket listener on its own thread. Returns immediately;
/// parsed commands arrive on `tx`. Dropping the returned guard removes
/// the socket file.
///
/// Fails with `ErrorKind::AddrInUse` if another daemon is already
/// accepting connections on `path`, rather than unlinking its socket out
/// from under it.
pub fn spawn(path: &str, group: &str, tx: SyncSender<SocketCommand>) -> io::Result<SocketGuard> {
    spawn_with(
        Path::new(path),
        group,
        Path::new(GROUP_FILE),
        tx,
        Limits::DEFAULT,
    )
}

fn spawn_with(
    path: &Path,
    group: &str,
    group_file: &Path,
    tx: SyncSender<SocketCommand>,
    limits: Limits,
) -> io::Result<SocketGuard> {
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another lcm-status daemon is already listening there",
        ));
    }
    let listener = bind_private(path, group, group_file)?;
    // From here on the socket file is ours to clean up, including if the
    // thread below can't start.
    let guard = SocketGuard::new(path);

    // One thread per connection, so a slow client (or a STATUS request
    // waiting on the main loop) never holds up anyone else's.
    thread::Builder::new()
        .name("socket-accept".into())
        .spawn(move || serve(listener.incoming(), &tx, limits))?;
    Ok(guard)
}

/// Where group names are looked up. A group that exists only in LDAP or
/// another NSS source isn't found here; the socket then stays root-only
/// and says so.
const GROUP_FILE: &str = "/etc/group";

/// The gid of group `name` in `/etc/group`-format `contents`
/// (`name:password:gid:members`), first match winning.
fn parse_group_gid(contents: &str, name: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let (group, _password, gid) = (fields.next()?, fields.next()?, fields.next()?);
        if group == name {
            gid.parse().ok()
        } else {
            None
        }
    })
}

/// Makes `path`'s group `group`. `Err` says why that didn't happen.
fn apply_group(path: &Path, group: &str, group_file: &Path) -> Result<(), String> {
    let contents = std::fs::read_to_string(group_file)
        .map_err(|e| format!("cannot read {}: {e}", group_file.display()))?;
    let gid = parse_group_gid(&contents, group)
        .ok_or_else(|| format!("no group \"{group}\" in {}", group_file.display()))?;
    std::os::unix::fs::chown(path, None, Some(gid))
        .map_err(|e| format!("chown to gid {gid} failed: {e}"))
}

/// Binds the listening socket at `path` with mode `0660` and group
/// `group` already in place when it first appears there.
///
/// `bind()` creates the socket file with whatever the umask leaves, and
/// std offers no way to set the umask (or `fchmod` a socket) safely. So
/// the socket is bound inside a fresh `0700` directory next to `path`,
/// where nobody else can reach it, given its mode and group there, and
/// only then `rename()`d into place (atomically replacing a stale one).
/// If that can't be done because the staging path is too long for a
/// socket address, it falls back to binding at `path` directly and
/// fixing the mode right after: a window of a few microseconds in which
/// the file has the umask's mode (root-owned either way).
fn bind_private(path: &Path, group: &str, group_file: &Path) -> io::Result<UnixListener> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no file name")
    })?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = parent.join(format!(
        ".{}.{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // Debris from an earlier run that crashed mid-way; any other problem
    // with the path shows up at the `create` below.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;

    let staged = staging.join("sock");
    let result = UnixListener::bind(&staged).and_then(|listener| {
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o660))?;
        if let Err(reason) = apply_group(&staged, group, group_file) {
            warn_root_only(path, group, &reason);
        }
        std::fs::rename(&staged, path)?;
        Ok(listener)
    });
    // Empty by now unless something above failed; best effort either way.
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        syslog::warning(&format!(
            "socket: could not remove {}: {e}",
            staging.display()
        ));
    }
    match result {
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => bind_in_place(path, group, group_file),
        other => other,
    }
}

/// The fallback for `bind_private`.
fn bind_in_place(path: &Path, group: &str, group_file: &Path) -> io::Result<UnixListener> {
    syslog::warning(&format!(
        "socket: {} is too long to stage privately; binding in place",
        path.display()
    ));
    let _ = std::fs::remove_file(path); // stale socket from a previous run
    let listener = UnixListener::bind(path)?;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)) {
        // Never leave it with whatever the umask allowed.
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    if let Err(reason) = apply_group(path, group, group_file) {
        warn_root_only(path, group, &reason);
    }
    Ok(listener)
}

fn warn_root_only(path: &Path, group: &str, reason: &str) {
    syslog::warning(&format!(
        "socket: {} stays root-only, not group \"{group}\": {reason}",
        path.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn parses_show_critical() {
        let cmd = parse_message(&lines("SHOW critical 0\nDISK FAILURE\nCheck bay 3")).unwrap();
        match cmd {
            SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0,
                line1,
            } => {
                assert_eq!(level, Level::Critical);
                assert_eq!(ttl_secs, 0);
                assert_eq!(bay, None);
                assert_eq!(line0, "DISK FAILURE");
                assert_eq!(line1, "Check bay 3");
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_show_with_bay() {
        let cmd = parse_message(&lines("SHOW error 0 bay=2\nDISK DEAD\nBay 2 offline")).unwrap();
        match cmd {
            SocketCommand::Show { level, bay, .. } => {
                assert_eq!(level, Level::Error);
                assert_eq!(bay, Some(2));
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_clear_with_bay() {
        let cmd = parse_message(&lines("CLEAR bay=2")).unwrap();
        match cmd {
            SocketCommand::Clear { bay } => assert_eq!(bay, Some(2)),
            _ => panic!("expected Clear"),
        }
    }

    fn locate(header: &str) -> Option<SocketCommand> {
        parse_message(&lines(header)).ok()
    }

    #[test]
    fn parses_locate_forms() {
        assert!(matches!(
            locate("LOCATE"),
            Some(SocketCommand::Locate {
                bay: None,
                ttl_secs: DEFAULT_LOCATE_TTL_SECS
            })
        ));
        assert!(matches!(
            locate("locate bay=3"),
            Some(SocketCommand::Locate {
                bay: Some(3),
                ttl_secs: DEFAULT_LOCATE_TTL_SECS
            })
        ));
        // Arguments in any order; 0 = until LOCATE off.
        assert!(matches!(
            locate("LOCATE 0 bay=2"),
            Some(SocketCommand::Locate {
                bay: Some(2),
                ttl_secs: 0
            })
        ));
        assert!(matches!(
            locate("LOCATE OFF"),
            Some(SocketCommand::LocateOff { bay: None })
        ));
        assert!(matches!(
            locate("LOCATE bay=4 off"),
            Some(SocketCommand::LocateOff { bay: Some(4) })
        ));
    }

    #[test]
    fn malformed_locate_is_rejected_not_widened_to_the_chassis() {
        assert!(locate("LOCATE bay=two").is_none());
        assert!(locate("LOCATE bay=0").is_none());
        assert!(locate("LOCATE bay=2 soon").is_none());
        assert!(locate("LOCATE off 30").is_none());
    }

    #[test]
    fn locate_request_round_trips() {
        for (bay, ttl, off) in [
            (None, None, false),
            (Some(2), None, false),
            (Some(1), Some(300), false),
            (None, Some(0), false),
            (None, None, true),
            (Some(3), None, true),
        ] {
            let parsed = locate(&locate_request(bay, ttl, off)).unwrap();
            match parsed {
                SocketCommand::Locate { bay: b, ttl_secs } => {
                    assert!(!off);
                    assert_eq!(b, bay);
                    assert_eq!(ttl_secs, ttl.unwrap_or(DEFAULT_LOCATE_TTL_SECS));
                }
                SocketCommand::LocateOff { bay: b } => {
                    assert!(off);
                    assert_eq!(b, bay);
                }
                _ => panic!("expected Locate/LocateOff"),
            }
        }
    }

    #[test]
    fn error_and_critical_are_always_visible() {
        assert!(Level::Error.always_visible());
        assert!(Level::Critical.always_visible());
        assert!(!Level::Warn.always_visible());
        assert!(!Level::Info.always_visible());
    }

    #[test]
    fn parses_bare_text_shorthand() {
        let cmd = parse_message(&lines("hello there\nsecond line")).unwrap();
        match cmd {
            SocketCommand::Show {
                level,
                ttl_secs,
                bay,
                line0,
                line1,
            } => {
                assert_eq!(level, Level::Info);
                assert_eq!(ttl_secs, 5);
                assert_eq!(bay, None);
                assert_eq!(line0, "hello there");
                assert_eq!(line1, "second line");
            }
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn parses_clear() {
        assert!(matches!(
            parse_message(&lines("CLEAR")),
            Ok(SocketCommand::Clear { bay: None })
        ));
    }

    fn temp_socket_path(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("lcm-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn refuses_to_take_over_a_live_socket() {
        let path = temp_socket_path("live");
        let _running = UnixListener::bind(&path).unwrap();
        let (tx, _rx) = command_channel();
        let err = spawn(&path, "nogroup", tx).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(std::path::Path::new(&path).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replaces_a_stale_socket() {
        let path = temp_socket_path("stale");
        // A leftover entry nothing is listening on. (A plain file rather
        // than a bound-then-dropped listener: on macOS a child spawned by
        // another test can inherit and hold the listener open.)
        std::fs::write(&path, "").unwrap();
        let (tx, _rx) = command_channel();
        let _guard = spawn(&path, "nogroup", tx).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stuck_client_does_not_block_others_and_is_eventually_dropped() {
        let path = temp_socket_path("stuck");
        let (tx, rx) = command_channel();
        let _guard = spawn(&path, "nogroup", tx).unwrap();

        // Connects, sends nothing, never closes.
        let mut stuck = UnixStream::connect(&path).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"hello\nworld\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let cmd = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("second client blocked behind the stuck one");
        assert!(matches!(cmd, SocketCommand::Show { line0, .. } if line0 == "hello"));

        // The daemon side gives up on the stuck client: we see EOF.
        stuck.set_read_timeout(Some(CLIENT_IO_TIMEOUT * 3)).unwrap();
        let mut buf = [0u8; 1];
        // Retried on EINTR: a signal aimed at another test thread (several
        // spawn and reap subprocesses) can interrupt this blocking read.
        let n = loop {
            match stuck.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                other => break other.unwrap(),
            }
        };
        assert_eq!(n, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bare_single_line_leaves_line1_blank() {
        let cmd = parse_message(&lines("just one line")).unwrap();
        match cmd {
            SocketCommand::Show { line1, .. } => assert_eq!(line1, ""),
            _ => panic!("expected Show"),
        }
    }

    // ---- parser strictness ------------------------------------------------

    fn parse_err(request: &str) -> String {
        match parse_request(&lines(request)) {
            Err(reason) => reason,
            Ok(_) => panic!("{request:?} should have been rejected"),
        }
    }

    #[test]
    fn levels_are_parsed_strictly() {
        assert_eq!(Level::parse("info"), Some(Level::Info));
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("Error"), Some(Level::Error));
        assert_eq!(Level::parse("critical"), Some(Level::Critical));
        assert_eq!(Level::parse("crtical"), None);
        assert_eq!(Level::parse("fatal"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn an_unknown_level_is_an_error_not_info() {
        let reason = parse_err("SHOW crtical 0\nDISK FAILURE");
        assert!(reason.contains("unknown level \"crtical\""), "{reason}");
        assert!(reason.contains("critical"), "{reason}");
    }

    #[test]
    fn incomplete_or_malformed_show_is_an_error() {
        assert!(parse_err("SHOW").contains("SHOW needs"));
        assert!(parse_err("SHOW error").contains("SHOW needs"));
        assert!(parse_err("SHOW error soon").contains("bad ttl_secs"));
        assert!(parse_err("SHOW error -1").contains("bad ttl_secs"));
        // A typo'd or extra argument is not quietly dropped.
        assert!(parse_err("SHOW error 0 bay=two").contains("bad bay"));
        assert!(parse_err("SHOW error 0 bay=0").contains("numbered from 1"));
        assert!(parse_err("SHOW error 0 baay=2").contains("unexpected argument"));
        assert!(parse_err("SHOW error 0 bay=1 bay=2").contains("twice"));
    }

    #[test]
    fn malformed_clear_is_not_widened_to_clear_everything() {
        assert!(parse_err("CLEAR bay=").contains("bad bay"));
        assert!(parse_err("CLEAR bay=0").contains("numbered from 1"));
        assert!(parse_err("CLEAR everything").contains("unexpected argument"));
        assert!(parse_request(&lines("clear bay=2")).is_ok());
    }

    #[test]
    fn locate_errors_say_what_was_wrong() {
        let reason = parse_err;
        assert!(reason("LOCATE bay=two").contains("bad bay"));
        assert!(reason("LOCATE bay=2 soon").contains("unexpected argument"));
        assert!(reason("LOCATE off 30").contains("no ttl"));
        assert!(reason("LOCATE 5 6").contains("twice"));
    }

    #[test]
    fn status_takes_no_arguments_and_is_never_bare_text() {
        assert!(matches!(
            parse_request(&lines("STATUS")),
            Ok(Request::Status)
        ));
        assert!(matches!(
            parse_request(&lines("  status  ")),
            Ok(Request::Status)
        ));
        assert!(parse_err("STATUS now").contains("no arguments"));
        assert!(parse_err("status verbose\nmore").contains("no arguments"));
    }

    #[test]
    fn a_blank_request_is_an_error() {
        assert_eq!(parse_err(""), "empty request");
        assert_eq!(parse_err("   \nsecond"), "empty request");
    }

    #[test]
    fn echoed_input_in_an_error_is_one_short_escaped_line() {
        let reason = parse_err("SHOW \u{7}bell\u{1b}[31mxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx 0");
        assert!(
            !reason.contains('\n') && !reason.contains('\u{1b}'),
            "{reason:?}"
        );
        assert!(reason.len() < 120, "{reason}");
    }

    // ---- over a connection ------------------------------------------------

    const FAST: Limits = Limits {
        max_connections: 16,
        io_timeout: Duration::from_millis(300),
        request_deadline: Duration::from_millis(700),
        reply_deadline: Duration::from_millis(700),
        accept_backoff: Duration::from_millis(30),
    };

    /// Runs one connection to completion on this thread and returns what
    /// the daemon wrote back.
    fn call(request: &[u8], tx: &SyncSender<SocketCommand>) -> String {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        handle_connection(&server, tx, FAST);
        drop(server);
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        String::from_utf8(reply).unwrap()
    }

    #[test]
    fn valid_fire_and_forget_requests_get_no_reply() {
        let (tx, rx) = command_channel();
        assert_eq!(call(b"SHOW warn 5\nA\nB\n", &tx), "");
        assert_eq!(call(b"CLEAR\n", &tx), "");
        assert_eq!(call(b"LOCATE bay=2\n", &tx), "");
        assert_eq!(call(b"bare text\n", &tx), "");
        assert_eq!(call(b"", &tx), "");
        assert_eq!(rx.try_iter().count(), 4);
    }

    #[test]
    fn malformed_requests_get_a_one_line_err_reply_and_queue_nothing() {
        let (tx, rx) = command_channel();
        let reply = call(b"SHOW crtical 0\nx\n", &tx);
        assert!(reply.starts_with("ERR unknown level"), "{reply}");
        assert!(reply.ends_with('\n') && reply.matches('\n').count() == 1);
        assert!(call(b"LOCATE bay=0\n", &tx).starts_with("ERR bad bay"));
        assert!(call(b"STATUS please\n", &tx).starts_with("ERR STATUS takes no arguments"));
        assert!(call(b"\n", &tx).starts_with("ERR empty request"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_truncated_message() {
        let (tx, rx) = command_channel();
        let reply = call(b"SHOW info 5\nbad \xff\xfe bytes\n", &tx);
        assert_eq!(reply, "ERR request is not valid UTF-8\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_client_that_never_reads_the_error_is_not_waited_on() {
        let (tx, _rx) = command_channel();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"SHOW nope 0\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let started = Instant::now();
        handle_connection(&server, &tx, FAST);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn an_unterminated_tail_is_not_acted_on_after_a_timeout() {
        // The client sends one whole line and half of another, then goes
        // quiet without closing. Only the whole line counts.
        let (tx, rx) = command_channel();
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"hello\nsec").unwrap();
        handle_connection(&server, &tx, FAST);
        match rx.try_recv().unwrap() {
            SocketCommand::Show { line0, line1, .. } => {
                assert_eq!(line0, "hello");
                assert_eq!(line1, "");
            }
            _ => panic!("expected Show"),
        }
        drop(client);
    }

    #[test]
    fn a_full_queue_drops_the_request_and_says_so() {
        let (tx, rx) = mpsc::sync_channel(2);
        assert_eq!(call(b"CLEAR\n", &tx), "");
        assert_eq!(call(b"CLEAR\n", &tx), "");
        let started = Instant::now();
        assert_eq!(
            call(b"CLEAR\n", &tx),
            "ERR busy: too many commands queued\n"
        );
        // Dropped, not waited on.
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(rx.try_iter().count(), 2);
        // Room again: accepted.
        assert_eq!(call(b"CLEAR\n", &tx), "");
    }

    #[test]
    fn a_full_queue_refuses_status_too() {
        let (tx, _rx) = mpsc::sync_channel(1);
        assert_eq!(call(b"CLEAR\n", &tx), "");
        let reply = call(b"STATUS\n", &tx);
        assert_eq!(reply, "ERR busy: too many commands queued\n");
    }

    #[test]
    fn a_gone_main_loop_is_reported() {
        let (tx, rx) = command_channel();
        drop(rx);
        assert_eq!(call(b"CLEAR\n", &tx), "ERR daemon is shutting down\n");
    }

    #[test]
    fn status_is_answered_by_the_main_loop() {
        let (tx, rx) = command_channel();
        let main_loop = thread::spawn(move || match rx.recv().unwrap() {
            SocketCommand::StatusRequest(reply) => reply.send("all good\n".into()).unwrap(),
            _ => panic!("expected StatusRequest"),
        });
        assert_eq!(call(b"STATUS\n", &tx), "all good\n");
        main_loop.join().unwrap();
    }

    #[test]
    fn the_main_loop_takes_a_bounded_batch_per_pass() {
        let (tx, rx) = command_channel();
        for _ in 0..COMMAND_QUEUE_DEPTH {
            tx.try_send(SocketCommand::Clear { bay: None }).unwrap();
        }
        // Full: one more is refused outright.
        assert!(tx.try_send(SocketCommand::Clear { bay: None }).is_err());
        assert_eq!(drain(&rx).count(), MAX_COMMANDS_PER_PASS);
        assert_eq!(drain(&rx).count(), MAX_COMMANDS_PER_PASS);
        assert_eq!(
            rx.try_iter().count(),
            COMMAND_QUEUE_DEPTH - 2 * MAX_COMMANDS_PER_PASS
        );
        assert_eq!(drain(&rx).count(), 0);
    }

    #[test]
    fn a_slow_drip_client_is_cut_off_at_the_deadline() {
        let (tx, rx) = command_channel();
        let (mut client, server) = UnixStream::pair().unwrap();
        let handler = thread::spawn(move || handle_connection(&server, &tx, FAST));

        // One byte every 100ms keeps beating the 300ms idle timeout; only
        // the overall deadline (700ms) can stop it.
        let started = Instant::now();
        let mut cut_off_after = None;
        for _ in 0..40 {
            if client.write_all(b"x").is_err() {
                cut_off_after = Some(started.elapsed());
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        handler.join().unwrap();
        let cut_off_after = cut_off_after.expect("the drip was never cut off");
        assert!(
            cut_off_after < Duration::from_millis(2000),
            "{cut_off_after:?}"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_slow_reader_does_not_hold_the_connection_past_the_reply_deadline() {
        let (tx, rx) = command_channel();
        let (client, server) = UnixStream::pair().unwrap();
        let (mut writer_side, _keep) = (client.try_clone().unwrap(), client);
        // A report far larger than the socket buffer, which nobody reads.
        let main_loop = thread::spawn(move || match rx.recv().unwrap() {
            SocketCommand::StatusRequest(reply) => reply.send("x".repeat(8 << 20)).unwrap(),
            _ => panic!("expected StatusRequest"),
        });
        writer_side.write_all(b"STATUS\n").unwrap();
        writer_side.shutdown(std::net::Shutdown::Write).unwrap();
        let started = Instant::now();
        handle_connection(&server, &tx, FAST);
        assert!(started.elapsed() < Duration::from_secs(3));
        main_loop.join().unwrap();
    }

    // ---- accept loop ------------------------------------------------------

    /// A listener stand-in: connections sent on the returned sender are
    /// "accepted" by a `serve` thread; dropping the sender ends it.
    fn fake_listener(
        tx: SyncSender<SocketCommand>,
        limits: Limits,
    ) -> (mpsc::Sender<io::Result<UnixStream>>, thread::JoinHandle<()>) {
        let (conns, incoming) = mpsc::channel();
        let server = thread::spawn(move || serve(incoming.into_iter(), &tx, limits));
        (conns, server)
    }

    #[test]
    fn connections_past_the_cap_are_turned_away_promptly() {
        let limits = Limits {
            max_connections: 2,
            io_timeout: Duration::from_secs(10),
            request_deadline: Duration::from_secs(10),
            ..FAST
        };
        let (tx, rx) = command_channel();
        let (conns, _server) = fake_listener(tx, limits);

        // Two clients that connect and sit there.
        let mut idle = Vec::new();
        for _ in 0..2 {
            let (client, server) = UnixStream::pair().unwrap();
            conns.send(Ok(server)).unwrap();
            idle.push(client);
        }
        // The third is told so at once, not after anyone's timeout.
        let (mut third, server) = UnixStream::pair().unwrap();
        third
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let started = Instant::now();
        conns.send(Ok(server)).unwrap();
        let mut reply = String::new();
        third.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "ERR busy: too many connections\n");
        assert!(started.elapsed() < Duration::from_secs(2));

        // Once the idle ones hang up, their places are free again.
        drop(idle);
        let mut got_through = false;
        for _ in 0..50 {
            let (mut client, server) = UnixStream::pair().unwrap();
            conns.send(Ok(server)).unwrap();
            client.write_all(b"hello\n").unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
                got_through = true;
                break;
            }
        }
        assert!(got_through, "no place freed up after the idle clients left");
    }

    #[test]
    fn accept_errors_are_backed_off_from_and_do_not_end_the_loop() {
        let (tx, rx) = command_channel();
        let (conns, server) = fake_listener(tx, FAST);
        let started = Instant::now();
        for _ in 0..3 {
            conns
                .send(Err(io::Error::from_raw_os_error(libc::EMFILE)))
                .unwrap();
        }
        // Interrupted/aborted accepts are retried at once, no pause.
        conns.send(Err(io::ErrorKind::Interrupted.into())).unwrap();
        let (mut client, accepted) = UnixStream::pair().unwrap();
        conns.send(Ok(accepted)).unwrap();
        client.write_all(b"after the errors\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(SocketCommand::Show { line0, .. }) if line0 == "after the errors"
        ));
        drop(conns);
        server.join().unwrap();
        // Three failures, each followed by the backoff.
        assert!(started.elapsed() >= 3 * FAST.accept_backoff);
    }

    // ---- socket file ------------------------------------------------------

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lcm-sock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const GROUPS: &str = "\
root:x:0:
# a comment, and a short line
daemon:x
staff:x:50:alice,bob
lcm-status:x:1234:lcm
lcm-status-extra:x:777:
lcm-status:x:9999:duplicate
broken:x:notanumber:
";

    #[test]
    fn groups_are_looked_up_in_etc_group_format() {
        assert_eq!(parse_group_gid(GROUPS, "root"), Some(0));
        assert_eq!(parse_group_gid(GROUPS, "staff"), Some(50));
        // Exact name (not a prefix), first entry wins.
        assert_eq!(parse_group_gid(GROUPS, "lcm-status"), Some(1234));
        assert_eq!(parse_group_gid(GROUPS, "lcm"), None);
        assert_eq!(parse_group_gid(GROUPS, "broken"), None);
        assert_eq!(parse_group_gid(GROUPS, "daemon"), None);
        assert_eq!(parse_group_gid(GROUPS, ""), None);
        assert_eq!(parse_group_gid("", "root"), None);
    }

    #[test]
    fn the_socket_is_chowned_to_the_configured_group() {
        let dir = scratch_dir("chown");
        let file = dir.join("target");
        std::fs::write(&file, "").unwrap();
        // A group this user can give a file to: the one it already has.
        let gid = std::fs::metadata(&file).unwrap().gid();
        let group_file = dir.join("group");
        std::fs::write(&group_file, format!("other:x:1:\ntest-group:x:{gid}:\n")).unwrap();
        assert_eq!(apply_group(&file, "test-group", &group_file), Ok(()));
        assert_eq!(std::fs::metadata(&file).unwrap().gid(), gid);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_group_that_cannot_be_applied_says_why() {
        let dir = scratch_dir("nogroup");
        let file = dir.join("target");
        std::fs::write(&file, "").unwrap();
        let group_file = dir.join("group");
        std::fs::write(&group_file, "root:x:0:\n").unwrap();
        let reason = apply_group(&file, "lcm-status", &group_file).unwrap_err();
        assert!(reason.contains("no group \"lcm-status\""), "{reason}");
        let reason = apply_group(&file, "root", &dir.join("missing")).unwrap_err();
        assert!(reason.contains("cannot read"), "{reason}");
        // A gid that parses but a file that isn't there: chown fails.
        std::fs::write(&group_file, "g:x:5:\n").unwrap();
        let reason = apply_group(&dir.join("gone"), "g", &group_file).unwrap_err();
        assert!(reason.contains("chown to gid 5 failed"), "{reason}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_socket_appears_with_mode_0660_and_no_staging_debris() {
        let dir = scratch_dir("mode");
        let path = dir.join("lcm.sock");
        let (tx, _rx) = command_channel();
        let guard = spawn_with(&path, "no-such-group", &dir.join("group"), tx, FAST).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);
        assert!(UnixStream::connect(&path).is_ok());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["lcm.sock"], "left over: {names:?}");
        drop(guard);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_path_too_long_to_stage_still_gets_a_0660_socket() {
        let dir = scratch_dir("long");
        // As long as a socket address allows, so the staging path (a few
        // characters longer) cannot fit.
        let room = 100usize.saturating_sub(dir.as_os_str().len() + 1);
        if room < 20 {
            std::fs::remove_dir_all(dir).unwrap();
            return; // scratch directory itself too deep to build the case
        }
        let path = dir.join("s".repeat(room));
        let (tx, _rx) = command_channel();
        let guard = spawn_with(&path, "no-such-group", &dir.join("group"), tx, FAST).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);
        assert!(UnixStream::connect(&path).is_ok());
        drop(guard);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_socket_file_is_removed_on_clean_shutdown() {
        let dir = scratch_dir("cleanup");
        let path = dir.join("lcm.sock");
        let (tx, _rx) = command_channel();
        let guard = spawn_with(&path, "x", &dir.join("group"), tx, FAST).unwrap();
        assert!(path.exists());
        drop(guard);
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_replacement_socket_is_not_removed_by_the_old_daemons_guard() {
        let dir = scratch_dir("replaced");
        let path = dir.join("lcm.sock");
        let (tx, _rx) = command_channel();
        let guard = spawn_with(&path, "x", &dir.join("group"), tx, FAST).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "someone else's").unwrap();
        drop(guard);
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
