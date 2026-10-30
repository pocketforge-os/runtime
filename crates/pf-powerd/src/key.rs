//! Physical PEK intake. Never grabs evdev, inhibits logind, or invokes a power command.
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const KEY_POWER: u16 = 116;
// Matches systemd-stable v252.39 src/login/logind-button.c, shipped in Build 58.
// Logind owns that timer and graceful poweroff, even when this process is absent.
pub const LONG_PRESS: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Press {
    down: Option<Duration>,
    desynced: bool,
}

impl Press {
    /// Kernel MONOTONIC event timestamps, not reader scheduling time, classify the hold.
    pub fn event(&mut self, kind: u16, code: u16, value: i32, at: Duration) -> bool {
        if kind == 0 && code == 3 {
            // SYN_DROPPED
            self.down = None;
            self.desynced = true;
        } else if self.desynced {
            if kind == 0 && code == 0 {
                self.desynced = false;
            }
        } else if kind == 1 && code == KEY_POWER {
            match value {
                1 if self.down.is_none() => self.down = Some(at),
                0 => {
                    return self
                        .down
                        .take()
                        .and_then(|start| at.checked_sub(start))
                        .is_some_and(|held| held < LONG_PRESS)
                }
                _ => {} // Repeat and duplicate down cannot restart the hold timer.
            }
        }
        false
    }
}

/// One coalesced request, retained across shell crashes/app sessions until presentation ACK.
/// The daemon incarnation prevents an old ACK from clearing a new daemon's request.
pub struct Menu {
    incarnation: String,
    sequence: u64,
    pending: Option<String>,
}

impl Menu {
    pub fn new(incarnation: String) -> Self {
        Self {
            incarnation,
            sequence: 0,
            pending: None,
        }
    }
    pub fn request(&mut self) {
        // Fail closed at exhaustion rather than reuse a request identity.
        if let Some(sequence) = self.sequence.checked_add(1) {
            self.sequence = sequence;
            self.pending = Some(format!("{}:{sequence}", self.incarnation));
        }
    }
    pub fn pending(&self) -> Option<String> {
        self.pending.clone()
    }
    pub fn acknowledge(&mut self, id: &str) -> bool {
        if self.pending.as_deref() != Some(id) {
            return false;
        }
        self.pending = None;
        true
    }
}

fn has_power(caps: &str) -> bool {
    let words: Option<Vec<u64>> = caps
        .split_whitespace()
        .rev()
        .map(|word| u64::from_str_radix(word, 16).ok())
        .collect();
    words
        .and_then(|words| {
            words
                .get(KEY_POWER as usize / usize::BITS as usize)
                .copied()
        })
        .is_some_and(|word| word & (1 << (u32::from(KEY_POWER) % usize::BITS)) != 0)
}

/// Bind to the board PMIC, never a USB keyboard or an application's virtual pad.
/// Multiple matching PEKs are refused rather than duplicating a physical press.
pub fn discover(sysfs: &Path, input: &Path) -> io::Result<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(sysfs)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("event") {
            continue;
        }
        let device = entry.path().join("device");
        if fs::read_to_string(device.join("name"))
            .unwrap_or_default()
            .trim()
            != "axp20x-pek"
        {
            continue;
        }
        if has_power(&fs::read_to_string(device.join("capabilities/key"))?) {
            found.push(input.join(name));
        }
    }
    if found.len() == 1 {
        Ok(found.remove(0))
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "expected one axp20x-pek KEY_POWER source, found {}",
                found.len()
            ),
        ))
    }
}

fn open(path: &Path) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    // Linux input.h EVIOCSCLOCKID = _IOW('E', 0xa0, int), valid on our aarch64/x86 hosts.
    let clock: libc::c_int = libc::CLOCK_MONOTONIC;
    // SAFETY: file is open; ioctl reads one initialized c_int for this fd only.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x4004_45a0, &clock) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn read_event(file: &mut File) -> io::Result<Option<libc::input_event>> {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll points to one initialized, live descriptor.
    let rc = unsafe { libc::poll(&mut poll, 1, 1000) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc == 0 {
        return Ok(None);
    }
    if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "PEK source disconnected",
        ));
    }
    let mut bytes = [0_u8; std::mem::size_of::<libc::input_event>()];
    if file.read(&mut bytes)? != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "incomplete PEK event",
        ));
    }
    // SAFETY: bytes contains exactly one input_event; all integer bit patterns are valid.
    Ok(Some(unsafe {
        std::ptr::read_unaligned(bytes.as_ptr().cast())
    }))
}

pub fn spawn(menu: Arc<Mutex<Menu>>) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-power-key".into())
        .spawn(move || {
            let mut last_error = String::new();
            loop {
                let result = (|| -> io::Result<()> {
                    let path = discover(Path::new("/sys/class/input"), Path::new("/dev/input"))?;
                    let mut file = open(&path)?;
                    let mut press = Press::default(); // Hotplug/reopen cannot inherit a held key.
                    eprintln!(
                        "pf-powerd: PEK intake ready {} (logind retains long press)",
                        path.display()
                    );
                    loop {
                        let event = match read_event(&mut file) {
                            Ok(Some(event)) => event,
                            Ok(None) => continue,
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                                ) =>
                            {
                                continue
                            }
                            Err(e) => return Err(e),
                        };
                        let (Ok(sec), Ok(usec)) = (
                            u64::try_from(event.time.tv_sec),
                            u32::try_from(event.time.tv_usec),
                        ) else {
                            press = Press::default();
                            continue;
                        };
                        if usec >= 1_000_000 {
                            press = Press::default();
                            continue;
                        }
                        if press.event(
                            event.type_,
                            event.code,
                            event.value,
                            Duration::new(sec, usec * 1000),
                        ) {
                            menu.lock().expect("power menu mutex poisoned").request();
                        }
                    }
                })();
                if let Err(error) = result {
                    let error = error.to_string();
                    if error != last_error {
                        eprintln!("pf-powerd: PEK intake unavailable: {error}; retrying");
                        last_error = error;
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(p: &mut Press, value: i32, ms: u64) -> bool {
        p.event(1, KEY_POWER, value, Duration::from_millis(ms))
    }
    #[test]
    fn short_repeat_long_unmatched_and_backwards_sequences() {
        let mut p = Press::default();
        assert!(!key(&mut p, 0, 0));
        assert!(!key(&mut p, 1, 10));
        assert!(!key(&mut p, 2, 100));
        assert!(!key(&mut p, 1, 200));
        assert!(key(&mut p, 0, 463));
        assert!(!key(&mut p, 0, 464));
        for duration in [5000, 5001, 10000] {
            key(&mut p, 1, 1000);
            key(&mut p, 1, 4999);
            assert!(!key(&mut p, 0, 1000 + duration));
        }
        key(&mut p, 1, 1000);
        assert!(!key(&mut p, 0, 999));
        key(&mut p, 1, 1000);
        assert!(key(&mut p, 0, 5999));
    }
    #[test]
    fn desync_and_reopen_discard_incomplete_holds() {
        let mut p = Press::default();
        key(&mut p, 1, 10);
        p.event(0, 3, 0, Duration::ZERO);
        assert!(!key(&mut p, 0, 20));
        key(&mut p, 1, 30);
        p.event(0, 0, 0, Duration::ZERO);
        assert!(!key(&mut p, 0, 40));
        key(&mut p, 1, 50);
        p = Press::default();
        assert!(!key(&mut p, 0, 60));
        key(&mut p, 1, 70);
        assert!(key(&mut p, 0, 80));
    }
    #[test]
    fn bounded_pending_survives_poll_crash_and_rejects_stale_ack() {
        let mut menu = Menu::new("boot-a".into());
        menu.request();
        let first = menu.pending().unwrap();
        assert_eq!(menu.pending().as_ref(), Some(&first)); // read != ACK / crashed shell
        menu.request();
        assert!(!menu.acknowledge(&first)); // racing new physical press
        let latest = menu.pending().unwrap();
        assert!(menu.acknowledge(&latest));
        assert_eq!(menu.pending(), None);
        let mut restarted = Menu::new("boot-b".into());
        restarted.request();
        assert!(!restarted.acknowledge(&first));
    }

    #[test]
    fn discovery_refuses_missing_ambiguous_and_non_pmic_sources() {
        let root =
            std::env::temp_dir().join(format!("pf-power-key-discovery-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let input = root.join("dev");
        let caps = if usize::BITS == 64 {
            "10000000000000 0"
        } else {
            "100000 0 0 0"
        };
        let add = |event: &str, name: &str, caps: &str| {
            let device = root.join(event).join("device");
            fs::create_dir_all(device.join("capabilities")).unwrap();
            fs::write(device.join("name"), name).unwrap();
            fs::write(device.join("capabilities/key"), caps).unwrap();
        };
        add("event0", "USB Keyboard", caps);
        add("event1", "axp20x-pek", "0");
        assert!(discover(&root, &input).is_err());
        add("event1", "axp20x-pek", caps);
        assert_eq!(discover(&root, &input).unwrap(), input.join("event1"));
        add("event2", "axp20x-pek", caps);
        assert!(discover(&root, &input).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
