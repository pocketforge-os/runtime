//! Hosting glue for the input broker: the key-event loop, the evdev key source, and the toast feed.
//!
//! The handler opens the volume-key node itself (never grabbed) and never touches the consumer's
//! re-emitted stream, so it is independent of which app, menu or first-run sheet has focus.

use crate::{Key, Outcome, PrefStore, ToastModel, VolumeKeys};
use pf_volumed::jack::RouteSource;
use pf_volumed::Output;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
const EV_KEY: u16 = 1;
const POLL_SLICE: Duration = Duration::from_millis(100);

/// One volume-key event: `value` 1 = down, 0 = up, 2 = kernel autorepeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    pub value: u8,
}

pub trait KeySource {
    /// Wait up to `timeout`; `Ok(None)` is a timeout, `Err` is a fatal source failure.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<KeyEvent>, String>;
}

pub trait ToastSink {
    fn show(&mut self, model: &ToastModel);
}

/// Run until `stop` or a source failure. `clock` is monotonic time since an arbitrary origin.
pub fn run<S, R, K, T>(
    volume: &mut VolumeKeys<S, R>,
    source: &mut K,
    sink: &mut T,
    clock: impl Fn() -> Duration,
    stop: &AtomicBool,
) -> Result<(), String>
where
    S: PrefStore,
    R: RouteSource,
    K: KeySource,
    T: ToastSink,
{
    while !stop.load(Ordering::Relaxed) {
        let event = source.next_event(POLL_SLICE)?;
        let now = clock();
        let mut outcomes = Vec::new();
        if let Some(KeyEvent { key, value }) = event {
            match value {
                1 => outcomes.extend(volume.press(key, now)),
                0 => volume.release(key),
                _ => {}
            }
        }
        outcomes.extend(volume.tick(now));
        for Outcome::Shown(model) in &outcomes {
            sink.show(model);
        }
    }
    Ok(())
}

/// Does the sysfs `capabilities/key` bitmap (hex words, highest first) include both volume keys?
pub fn advertises_volume_keys(key_caps: &str, word_bits: u32) -> bool {
    let words: Option<Vec<u64>> = key_caps
        .split_whitespace()
        .rev()
        .map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    let Some(words) = words else { return false };
    let bit = |code: u16| {
        let (word, offset) = (
            usize::from(code) / word_bits as usize,
            u32::from(code) % word_bits,
        );
        words.get(word).is_some_and(|w| w & (1 << offset) != 0)
    };
    bit(KEY_VOLUMEDOWN) && bit(KEY_VOLUMEUP)
}

/// Find the evdev node advertising both volume keys. Zero or several candidates is an error.
pub fn discover_keys(sysfs_input: &Path, dev_input: &Path) -> Result<PathBuf, String> {
    let mut hits = Vec::new();
    for entry in fs::read_dir(sysfs_input)
        .map_err(|e| format!("read {}: {e}", sysfs_input.display()))?
        .flatten()
    {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| n.starts_with("event")) else {
            continue;
        };
        if let Ok(caps) = fs::read_to_string(entry.path().join("device/capabilities/key")) {
            if advertises_volume_keys(&caps, usize::BITS) {
                hits.push(dev_input.join(name));
            }
        }
    }
    hits.sort();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(String::from(
            "no input node advertises KEY_VOLUMEUP and KEY_VOLUMEDOWN",
        )),
        n => Err(format!("{n} input nodes advertise the volume keys")),
    }
}

pub struct EvdevKeys {
    file: File,
}

impl EvdevKeys {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        Ok(Self { file })
    }
}

impl KeySource for EvdevKeys {
    fn next_event(&mut self, timeout: Duration) -> Result<Option<KeyEvent>, String> {
        let mut poll = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised pollfd, count 1.
        let rc = unsafe {
            libc::poll(
                &mut poll,
                1,
                i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX),
            )
        };
        if rc < 0 {
            return Err(format!("poll: {}", std::io::Error::last_os_error()));
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(String::from("volume-key device went away"));
        }
        if rc == 0 {
            return Ok(None);
        }
        let mut raw = [0_u8; std::mem::size_of::<libc::input_event>()];
        match (&self.file).read(&mut raw) {
            Ok(n) if n == raw.len() => {}
            Ok(0) => return Err(String::from("volume-key device closed")),
            Ok(_) => return Err(String::from("partial input event")),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(format!("read: {e}")),
        }
        // SAFETY: `raw` is exactly one input_event worth of bytes; the struct is plain data.
        let event: libc::input_event = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast()) };
        let key = match event.code {
            KEY_VOLUMEUP => Key::Up,
            KEY_VOLUMEDOWN => Key::Down,
            _ => return Ok(None),
        };
        Ok((event.type_ == EV_KEY).then_some(KeyEvent {
            key,
            value: event.value.clamp(0, 2) as u8,
        }))
    }
}

/// One feed line per toast, for the OSD process (tsp-ihne1.63.1 binds it to the overlay).
pub fn encode_feed(model: &ToastModel) -> String {
    match model {
        ToastModel::Level {
            percent,
            output,
            muted,
        } => {
            let out = if *output == Output::Speaker {
                "speaker"
            } else {
                "headphones"
            };
            format!("level {percent} {out} {}\n", u8::from(*muted))
        }
        ToastModel::Error { message } => format!("error {}\n", message.replace(['\n', '\r'], " ")),
    }
}

pub fn decode_feed(line: &str) -> Result<ToastModel, String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if let Some(message) = line.strip_prefix("error ") {
        return Ok(ToastModel::Error {
            message: message.to_owned(),
        });
    }
    let parts: Vec<&str> = line.split(' ').collect();
    match parts.as_slice() {
        ["level", percent, output, muted] => Ok(ToastModel::Level {
            percent: percent
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= 100)
                .ok_or("bad percent")?,
            output: match *output {
                "speaker" => Output::Speaker,
                "headphones" => Output::Headphones,
                _ => return Err(String::from("bad output")),
            },
            muted: match *muted {
                "0" => false,
                "1" => true,
                _ => return Err(String::from("bad muted flag")),
            },
        }),
        _ => Err(format!("unrecognised feed line {line:?}")),
    }
}

/// Best-effort datagram feed; a missing or slow OSD never blocks or fails key handling.
pub struct SocketSink {
    socket: UnixDatagram,
    path: PathBuf,
}

impl SocketSink {
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let socket = UnixDatagram::unbound()?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            path: path.into(),
        })
    }
}

impl ToastSink for SocketSink {
    fn show(&mut self, model: &ToastModel) {
        let _ = self
            .socket
            .send_to(encode_feed(model).as_bytes(), &self.path);
    }
}

/// Route source that (re)discovers the codec jack lazily, so a late or vanished node is an
/// error for that key press (fail-safe error toast, no write) instead of a dead handler.
pub struct LazyJack {
    sysfs: PathBuf,
    dev: PathBuf,
    jack: Option<pf_volumed::jack::EvdevJack>,
}

impl LazyJack {
    pub fn new(sysfs: impl Into<PathBuf>, dev: impl Into<PathBuf>) -> Self {
        Self {
            sysfs: sysfs.into(),
            dev: dev.into(),
            jack: None,
        }
    }
}

impl RouteSource for LazyJack {
    fn route(&mut self) -> Result<Output, pf_volumed::jack::RouteError> {
        use pf_volumed::jack::{discover, EvdevJack, RouteError};
        if self.jack.is_none() {
            let node = discover(&self.sysfs, &self.dev)?
                .ok_or_else(|| RouteError::new("headphone-jack switch absent"))?;
            self.jack = Some(EvdevJack::open(&node)?);
        }
        let result = self.jack.as_mut().expect("opened above").route();
        if result.is_err() {
            self.jack = None;
        }
        result
    }
}

/// The service main loop: (re)discover the key node, run the handler, retry on failure, until
/// `stop`. Discovery errors are retried so a late node does not need a service restart.
pub fn serve(prefs_sock: &Path, toast_sock: Option<&Path>, stop: &AtomicBool) {
    use crate::{PrefsdStore, VolumeKeys};
    struct Null;
    impl ToastSink for Null {
        fn show(&mut self, _: &ToastModel) {}
    }
    let started = std::time::Instant::now();
    let mut last = String::new();
    while !stop.load(Ordering::Relaxed) {
        let source = discover_keys(Path::new("/sys/class/input"), Path::new("/dev/input"))
            .and_then(|n| EvdevKeys::open(&n));
        let mut source = match source {
            Ok(source) => source,
            Err(error) => {
                if last != error {
                    eprintln!("pf-volume-keys: waiting for the key device: {error}");
                    last = error;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let mut volume = VolumeKeys::new(
            PrefsdStore(pf_prefsd::Client::new(prefs_sock)),
            LazyJack::new("/sys/class/input", "/dev/input"),
        );
        let result = match toast_sock.map(SocketSink::new) {
            Some(Ok(mut sink)) => run(
                &mut volume,
                &mut source,
                &mut sink,
                || started.elapsed(),
                stop,
            ),
            _ => run(
                &mut volume,
                &mut source,
                &mut Null,
                || started.elapsed(),
                stop,
            ),
        };
        if let Err(error) = result {
            eprintln!("pf-volume-keys: stopped, retrying: {error}");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

pub const DEFAULT_PREFS_SOCK: &str = "/run/pocketforge/prefsd.sock";

/// The pf-prefsd endpoint: `--prefs-sock`, else the `PF_PREFSD_SOCK` override every other
/// preference client honours, else the default. An empty override is ignored.
pub fn resolve_prefs_sock(flag: Option<PathBuf>, env: Option<std::ffi::OsString>) -> PathBuf {
    flag.or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PREFS_SOCK))
}
