//! Wake-on-LAN: keeps `[wol] mode` (default magic packet) enabled on the
//! `[wol] nics` listed in config, and reports every NIC's WOL state for
//! `STATUS`.
//!
//! Talks to the driver directly with the `SIOCETHTOOL` ioctl
//! (`ETHTOOL_GWOL`/`ETHTOOL_SWOL`, the same calls `ethtool -s IFACE wol g`
//! makes), so it doesn't depend on `ethtool` being installed or on parsing
//! its output. Nothing about WOL persists across a reboot on its own, and
//! TrueNAS doesn't manage it, so this re-applies it at startup, rechecks
//! it every `RECHECK_INTERVAL` (some drivers reset it on link changes or
//! a driver reload), and applies it once more as the daemon exits --
//! which, during a shutdown, is shortly before power-off.
//!
//! The NIC only has to arm WOL; whether the board actually powers back up
//! on a magic packet is the BIOS's call (ErP/EuP off, wake on PCIe/LAN
//! enabled) -- see the README.

use crate::config::WolConfig;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

/// Wake on PHY activity (link change).
const WAKE_PHY: u32 = 1 << 0;
/// Wake on unicast frames to this NIC.
const WAKE_UCAST: u32 = 1 << 1;
/// Wake on multicast frames.
const WAKE_MCAST: u32 = 1 << 2;
/// Wake on broadcast frames.
const WAKE_BCAST: u32 = 1 << 3;
/// Wake on ARP.
const WAKE_ARP: u32 = 1 << 4;
/// Wake on a magic packet.
const WAKE_MAGIC: u32 = 1 << 5;
/// Magic packet with a `SecureOn` password -- reported, never set here.
const WAKE_MAGICSECURE: u32 = 1 << 6;
/// Wake on a filter match -- reported, never set here.
const WAKE_FILTER: u32 = 1 << 7;

/// `ethtool`'s letter for each `WAKE_*` bit, in `ethtool`'s own order.
const FLAG_LETTERS: [(char, u32); 8] = [
    ('p', WAKE_PHY),
    ('u', WAKE_UCAST),
    ('m', WAKE_MCAST),
    ('b', WAKE_BCAST),
    ('a', WAKE_ARP),
    ('g', WAKE_MAGIC),
    ('s', WAKE_MAGICSECURE),
    ('f', WAKE_FILTER),
];

/// How often configured NICs are rechecked (one cheap ioctl each) and
/// WOL re-applied if something reset it.
const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Parses an `ethtool`-style wake mode (`"g"`, `"pg"`, ...; `"d"` = WOL
/// off) into `WAKE_*` bits. `s` (needs a `SecureOn` password) and `f`
/// (needs filters set up) aren't accepted: there's nowhere to configure
/// what they depend on.
pub fn parse_mode(mode: &str) -> Result<u32, String> {
    if mode == "d" {
        return Ok(0);
    }
    if mode.is_empty() {
        return Err("empty".to_string());
    }
    mode.chars().try_fold(0, |bits, c| {
        match FLAG_LETTERS.iter().find(|(l, _)| *l == c) {
            Some((_, bit)) if !matches!(c, 's' | 'f') => Ok(bits | bit),
            _ => Err(format!(
                "'{c}' isn't supported (use any of p, u, m, b, a, g -- or \"d\" to disable)"
            )),
        }
    })
}

/// `WAKE_*` bits as `ethtool` prints them (`"g"`, `"pumbg"`; `"d"` for none).
pub fn format_mode(bits: u32) -> String {
    if bits == 0 {
        return "d".to_string();
    }
    FLAG_LETTERS
        .iter()
        .filter(|(_, bit)| bits & bit != 0)
        .map(|(l, _)| *l)
        .collect()
}

/// One NIC's WOL capabilities and current setting, as `ETHTOOL_GWOL`
/// reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WolInfo {
    pub supported: u32,
    pub enabled: u32,
}

/// Reads `iface`'s current WOL state.
pub fn get(iface: &str) -> io::Result<WolInfo> {
    sys::get(iface)
}

/// One line per interface for the `STATUS` report: current mode, what it
/// supports, and -- if it's one of `[wol] nics` -- what it's being kept at.
pub fn describe(iface: &str, cfg: &WolConfig) -> String {
    let wanted = cfg
        .nics
        .iter()
        .any(|n| n == iface)
        .then(|| parse_mode(&cfg.mode).ok())
        .flatten();
    let state = match get(iface) {
        Ok(info) if info.supported == 0 => "not supported".to_string(),
        Ok(info) => format!(
            "{:<6} (supports {})",
            format_mode(info.enabled),
            format_mode(info.supported)
        ),
        Err(e) => format!("unknown ({e})"),
    };
    match wanted {
        Some(bits) => format!(
            "{iface:<10} {state}  managed: keeping at {}",
            format_mode(bits)
        ),
        None => format!("{iface:<10} {state}"),
    }
}

/// What `WolKeeper` found for one NIC.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Already set as configured.
    Unchanged,
    /// Was something else (shown); has been set as configured.
    Applied { was: u32 },
    /// Couldn't be set; why.
    Failed(String),
}

/// Keeps `[wol] mode` applied to every `[wol] nics` interface. Owned by
/// the main loop; every check is one ioctl per NIC, so it never blocks.
pub struct WolKeeper {
    nics: Vec<String>,
    want: u32,
    last_check: Option<Instant>,
    /// NICs that have been brought to `want` at least once since startup,
    /// so a later reset reads as "something changed it", not first setup.
    applied: Vec<String>,
    /// The problem last logged per NIC, so each is logged once (and its
    /// recovery once), not on every recheck.
    problems: HashMap<String, String>,
}

impl WolKeeper {
    /// A keeper for `cfg`. `Config::validate` has already reported (and
    /// cleared `nics` for) an unparseable mode, so that case does nothing.
    pub fn new(cfg: &WolConfig) -> Self {
        let (nics, want) = match parse_mode(&cfg.mode) {
            Ok(want) => (cfg.nics.clone(), want),
            Err(_) => (Vec::new(), 0),
        };
        WolKeeper {
            nics,
            want,
            last_check: None,
            applied: Vec::new(),
            problems: HashMap::new(),
        }
    }

    /// `enforce`, if `RECHECK_INTERVAL` has passed since the last check
    /// (or there hasn't been one). Call on every main-loop wakeup.
    pub fn maybe_enforce(&mut self) {
        if self
            .last_check
            .is_none_or(|t| t.elapsed() >= RECHECK_INTERVAL)
        {
            self.enforce();
        }
    }

    /// Checks every configured NIC now, re-applying WOL wherever it
    /// isn't what's configured, and logs what changed.
    pub fn enforce(&mut self) {
        self.last_check = Some(Instant::now());
        for iface in self.nics.clone() {
            let outcome = enforce_one(&iface, self.want, sys::get, sys::set);
            self.log(&iface, outcome);
        }
    }

    fn log(&mut self, iface: &str, outcome: Outcome) {
        let mode = format_mode(self.want);
        if let Outcome::Failed(problem) = outcome {
            if self.problems.get(iface) != Some(&problem) {
                crate::syslog::warning(&format!(
                    "{iface}: couldn't set Wake-on-LAN to {mode}: {problem}"
                ));
                self.problems.insert(iface.to_string(), problem);
            }
            return;
        }
        if self.problems.remove(iface).is_some() {
            crate::syslog::notice(&format!(
                "{iface}: Wake-on-LAN {mode} applied normally again"
            ));
        }
        let first = !self.applied.iter().any(|n| n == iface);
        if first {
            self.applied.push(iface.to_string());
        }
        match outcome {
            Outcome::Applied { was } if first => crate::syslog::info(&format!(
                "{iface}: Wake-on-LAN set to {mode} (was {})",
                format_mode(was)
            )),
            Outcome::Applied { was } => crate::syslog::notice(&format!(
                "{iface}: Wake-on-LAN had been reset to {}, re-applied {mode}",
                format_mode(was)
            )),
            Outcome::Unchanged | Outcome::Failed(_) => {}
        }
    }
}

/// Brings one NIC to `want`, through `get`/`set` (the ioctl wrappers, or
/// fakes in tests), and confirms it by reading back.
fn enforce_one(
    iface: &str,
    want: u32,
    get: impl Fn(&str) -> io::Result<WolInfo>,
    set: impl Fn(&str, u32) -> io::Result<()>,
) -> Outcome {
    let info = match get(iface) {
        Ok(info) => info,
        Err(e) => return Outcome::Failed(format!("reading WOL state: {e}")),
    };
    if info.enabled == want {
        return Outcome::Unchanged;
    }
    let missing = want & !info.supported;
    if missing != 0 {
        return Outcome::Failed(format!(
            "NIC doesn't support {} (supports {})",
            format_mode(missing),
            format_mode(info.supported)
        ));
    }
    if let Err(e) = set(iface, want) {
        return Outcome::Failed(format!("setting WOL: {e}"));
    }
    match get(iface) {
        Ok(after) if after.enabled == want => Outcome::Applied { was: info.enabled },
        Ok(after) => Outcome::Failed(format!(
            "driver accepted it but reports {}",
            format_mode(after.enabled)
        )),
        Err(e) => Outcome::Failed(format!("reading WOL state back: {e}")),
    }
}

/// The `SIOCETHTOOL` calls themselves -- Linux only; elsewhere (tests on a
/// dev machine) every call fails as unsupported.
#[cfg(target_os = "linux")]
mod sys {
    use super::WolInfo;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// From `<linux/sockios.h>`; spelled out here because `libc` types it
    /// as `c_ulong` while musl's `ioctl` takes a `c_int` request.
    const SIOCETHTOOL: libc::Ioctl = 0x8946;
    /// From `<linux/ethtool.h>`.
    const ETHTOOL_GWOL: u32 = 0x0000_0005;
    const ETHTOOL_SWOL: u32 = 0x0000_0006;

    /// `struct ethtool_wolinfo` from `<linux/ethtool.h>`.
    #[repr(C)]
    #[derive(Default)]
    struct EthtoolWolinfo {
        cmd: u32,
        supported: u32,
        wolopts: u32,
        sopass: [u8; 6],
    }

    pub fn get(iface: &str) -> io::Result<WolInfo> {
        let mut info = EthtoolWolinfo {
            cmd: ETHTOOL_GWOL,
            ..EthtoolWolinfo::default()
        };
        ethtool(iface, &mut info)?;
        Ok(WolInfo {
            supported: info.supported,
            enabled: info.wolopts,
        })
    }

    pub fn set(iface: &str, wolopts: u32) -> io::Result<()> {
        let mut info = EthtoolWolinfo {
            cmd: ETHTOOL_SWOL,
            wolopts,
            ..EthtoolWolinfo::default()
        };
        ethtool(iface, &mut info)
    }

    /// Issues one `SIOCETHTOOL` request for `iface` on a throwaway socket.
    fn ethtool(iface: &str, info: &mut EthtoolWolinfo) -> io::Result<()> {
        let name = iface.as_bytes();
        if name.is_empty() || name.len() >= libc::IFNAMSIZ || name.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad interface name",
            ));
        }
        // SAFETY: plain socket(2) call; the result is checked before use.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a valid descriptor just returned by socket(2),
        // owned by nothing else; `OwnedFd` closes it on drop.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };

        // SAFETY: `ifreq` is plain data (a name array and a union of plain
        // data / a pointer), for which all-zeroes is valid.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        // Leaves the zeroed NUL terminator in place: `name` is shorter
        // than `IFNAMSIZ`, checked above.
        for (dst, &src) in ifr.ifr_name.iter_mut().zip(name) {
            *dst = libc::c_char::from_ne_bytes([src]);
        }
        ifr.ifr_ifru.ifru_data = std::ptr::from_mut(info).cast::<libc::c_char>();

        // SAFETY: SIOCETHTOOL takes a `struct ifreq *` whose `ifr_data`
        // points at an ethtool command struct; `ifr` and `*info` (the
        // `ethtool_wolinfo` that GWOL/SWOL read and write) both outlive
        // the call.
        let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCETHTOOL, &raw mut ifr) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use super::WolInfo;
    use std::io;

    pub fn get(_iface: &str) -> io::Result<WolInfo> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn set(_iface: &str, _wolopts: u32) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// What `ethtool enp2s0` showed live on the AS6704T's RTL8125s.
    const RTL8125_SUPPORTS: u32 = WAKE_PHY | WAKE_UCAST | WAKE_MCAST | WAKE_BCAST | WAKE_MAGIC;

    #[test]
    fn modes_round_trip_in_ethtool_notation() {
        assert_eq!(parse_mode("g"), Ok(WAKE_MAGIC));
        assert_eq!(parse_mode("d"), Ok(0));
        assert_eq!(parse_mode("gp"), Ok(WAKE_MAGIC | WAKE_PHY));
        assert_eq!(format_mode(RTL8125_SUPPORTS), "pumbg");
        assert_eq!(format_mode(0), "d");
    }

    #[test]
    fn unusable_modes_are_rejected() {
        assert!(parse_mode("").is_err());
        assert!(parse_mode("s").is_err()); // needs a SecureOn password
        assert!(parse_mode("gx").is_err());
        assert!(parse_mode("dg").is_err());
    }

    /// A fake NIC: `get` reports `enabled`, `set` updates it unless
    /// `sticky` (a driver that silently ignores the request).
    fn nic(enabled: u32, sticky: bool) -> (Cell<u32>, bool) {
        (Cell::new(enabled), sticky)
    }

    fn run(fake: &(Cell<u32>, bool), want: u32) -> Outcome {
        enforce_one(
            "enp2s0",
            want,
            |_| {
                Ok(WolInfo {
                    supported: RTL8125_SUPPORTS,
                    enabled: fake.0.get(),
                })
            },
            |_, v| {
                if !fake.1 {
                    fake.0.set(v);
                }
                Ok(())
            },
        )
    }

    #[test]
    fn disabled_wol_gets_enabled_and_confirmed() {
        let fake = nic(0, false);
        assert_eq!(run(&fake, WAKE_MAGIC), Outcome::Applied { was: 0 });
        assert_eq!(fake.0.get(), WAKE_MAGIC);
        assert_eq!(run(&fake, WAKE_MAGIC), Outcome::Unchanged);
    }

    #[test]
    fn unsupported_mode_is_refused_without_writing() {
        let fake = nic(0, false);
        let outcome = run(&fake, WAKE_ARP | WAKE_MAGIC);
        assert!(
            matches!(&outcome, Outcome::Failed(m) if m.contains("doesn't support a")),
            "{outcome:?}"
        );
        assert_eq!(fake.0.get(), 0);
    }

    #[test]
    fn a_driver_ignoring_the_request_is_caught_by_read_back() {
        let fake = nic(0, true);
        assert!(matches!(run(&fake, WAKE_MAGIC), Outcome::Failed(_)));
    }

    #[test]
    fn invalid_mode_in_config_manages_nothing() {
        let keeper = WolKeeper::new(&WolConfig {
            nics: vec!["enp2s0".into()],
            mode: "x".into(),
        });
        assert_eq!(keeper.nics, Vec::<String>::new());
    }
}
