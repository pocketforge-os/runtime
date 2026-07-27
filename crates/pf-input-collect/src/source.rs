//! The **event source** the collection engine reads — abstracted behind a trait so the engine
//! is identical whether it is driven by a real `/dev/input/eventN` node on the device or by a
//! synthetic, scripted stream in a unit test.
//!
//! The real source reads the raw node DIRECTLY (post-decoder), exactly as
//! `evdev-probe.py --watch` does — it deliberately does NOT go through `pf-input-broker`
//! (whose `InputBroker::start_with()` needs a `Descriptor` the guided-collection mode has none
//! of). The ioctl plumbing here is self-contained and follows the `pf-hw-exerciser` precedent:
//! a minimal `_IOC` table typed as `libc::Ioctl` so the crate cross-compiles cleanly to
//! `aarch64-unknown-linux-musl` (static device binary) as well as the native build/test target.
//! Keeping our own copy also decouples this crate from in-flight edits to `pf-input-broker`.

use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

use libc::{c_ulong, Ioctl};

use crate::codes::{EV_ABS, EV_KEY};

// --- asm-generic _IOC encoding (identical on aarch64 + x86_64) -------------------------------
const IOC_READ: c_ulong = 2;

/// `_IOC(dir, type, nr, size)` truncated to `libc::Ioctl` (i32 on musl, u64 on gnu) preserving
/// the bit pattern — same construction `pf-hw-exerciser` and `evdev-probe.py` use.
const fn ioc(dir: c_ulong, ty: u8, nr: c_ulong, size: c_ulong) -> Ioctl {
    ((dir << 30) | (size << 16) | ((ty as c_ulong) << 8) | nr) as Ioctl
}

const EV: u8 = b'E';

fn eviocgname(len: usize) -> Ioctl {
    ioc(IOC_READ, EV, 0x06, len as c_ulong)
}
fn eviocgid() -> Ioctl {
    ioc(IOC_READ, EV, 0x02, std::mem::size_of::<libc::input_id>() as c_ulong)
}
fn eviocgabs(abs: u16) -> Ioctl {
    ioc(IOC_READ, EV, 0x40 + abs as c_ulong, std::mem::size_of::<libc::input_absinfo>() as c_ulong)
}

/// One decoded evdev event: the `(ev_type, code, value)` triple the engine records per prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

impl RawEvent {
    pub fn new(ev_type: u16, code: u16, value: i32) -> RawEvent {
        RawEvent { ev_type, code, value }
    }
}

/// A `struct input_absinfo` read via `EVIOCGABS` — the driver-declared range for an axis.
///
/// This is the GROUND-TRUTH declared range (min/max/fuzz/flat), which is what the descriptor's
/// `range`/`x`/`y` carry — distinct from the values a user actually sweeps to (which the engine
/// also tracks, for calibration + binary/analog classification).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// Device identity read via `EVIOCGNAME` (`name`) + `EVIOCGID` (`bus`/`vid`/`pid`/`version`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub bus: u16,
    pub vid: u16,
    pub pid: u16,
    pub version: u16,
}

/// A source of decoded evdev events plus the device's identity + per-axis declared ranges.
pub trait EventSource {
    /// `EVIOCGNAME` + `EVIOCGID`.
    fn identity(&mut self) -> io::Result<Identity>;
    /// `EVIOCGABS(abs_code)` — the declared range for an absolute axis.
    fn absinfo(&mut self, abs_code: u16) -> io::Result<AbsInfo>;
    /// Block up to `timeout` for events; return every event that became available (possibly
    /// empty if the timeout elapsed with nothing pending). `EV_SYN` boundaries are filtered
    /// out by the source — the engine only ever sees `EV_KEY`/`EV_ABS` payload events.
    fn poll(&mut self, timeout: Duration) -> io::Result<Vec<RawEvent>>;

    /// Route subsequent `poll`/`absinfo` calls to the node named `node` (a control's descriptor
    /// `source`), or to the PRIMARY node when `None` (tsp-bwrg.16). The engine calls this before
    /// pumping each control so a multi-node device reads each control from the right evdev node.
    ///
    /// DEFAULT = NO-OP: a single-node source (the live [`EvdevSource`], the [`ScriptedSource`])
    /// has exactly one node and ignores the hint, so single-source collection is unchanged. Only
    /// [`MultiSource`] overrides this to actually switch nodes. `identity()` is UNAFFECTED — it
    /// always reports the device's PRIMARY-node identity regardless of the active routing.
    fn set_active_source(&mut self, _node: Option<&str>) {}
}

// -------------------------------------------------------------------------------------------
// Real source: a live /dev/input/eventN node.
// -------------------------------------------------------------------------------------------

/// The live-device source: an opened evdev node read directly (NOT grabbed — guided collection
/// is a passive observer of what the decoder emits, and grabbing would fight any other reader).
pub struct EvdevSource {
    fd: OwnedFd,
    buf: Vec<libc::input_event>,
}

/// A minimal owned fd wrapper (close-on-drop) — avoids pulling a heavier dependency for one fd.
struct OwnedFd(RawFd);
impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: self.0 is a valid fd owned by this wrapper.
            unsafe { libc::close(self.0) };
        }
    }
}

impl EvdevSource {
    /// Open `path` (e.g. `/dev/input/event3`) for guided collection.
    pub fn open(path: impl AsRef<std::path::Path>) -> io::Result<EvdevSource> {
        let cpath = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
        // SAFETY: cpath is a valid NUL-terminated C string for the duration of the call.
        let raw = unsafe {
            libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(EvdevSource {
            fd: OwnedFd(raw),
            buf: vec![unsafe { std::mem::zeroed() }; 64],
        })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.0
    }
}

impl EventSource for EvdevSource {
    fn identity(&mut self) -> io::Result<Identity> {
        // EVIOCGNAME.
        let mut nbuf = [0u8; 256];
        // SAFETY: nbuf is a valid writable buffer of the length passed to the ioctl.
        let n = unsafe { libc::ioctl(self.raw_fd(), eviocgname(nbuf.len()), nbuf.as_mut_ptr()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let end = nbuf[..(n as usize).min(nbuf.len())]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or((n as usize).min(nbuf.len()));
        let name = String::from_utf8_lossy(&nbuf[..end]).trim_end().to_string();

        // EVIOCGID.
        let mut id: libc::input_id = unsafe { std::mem::zeroed() };
        // SAFETY: &mut id is a valid input_id-sized buffer matching the ioctl's size field.
        let rc = unsafe { libc::ioctl(self.raw_fd(), eviocgid(), &mut id as *mut libc::input_id) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Identity {
            name,
            bus: id.bustype,
            vid: id.vendor,
            pid: id.product,
            version: id.version,
        })
    }

    fn absinfo(&mut self, abs_code: u16) -> io::Result<AbsInfo> {
        let mut ai: libc::input_absinfo = unsafe { std::mem::zeroed() };
        // SAFETY: &mut ai is a valid input_absinfo-sized buffer matching the ioctl's size field.
        let rc = unsafe {
            libc::ioctl(self.raw_fd(), eviocgabs(abs_code), &mut ai as *mut libc::input_absinfo)
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AbsInfo {
            min: ai.minimum,
            max: ai.maximum,
            fuzz: ai.fuzz,
            flat: ai.flat,
            resolution: ai.resolution,
        })
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<Vec<RawEvent>> {
        let mut pfd = libc::pollfd { fd: self.raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms: i32 = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: pfd points to one valid pollfd; count = 1.
        let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(Vec::new());
            }
            return Err(e);
        }
        if rc == 0 {
            return Ok(Vec::new()); // timed out
        }
        let cap = std::mem::size_of_val(&self.buf[..]);
        // SAFETY: buf is a valid writable buffer of `cap` bytes; read writes at most `cap`.
        let n =
            unsafe { libc::read(self.raw_fd(), self.buf.as_mut_ptr() as *mut libc::c_void, cap) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(Vec::new());
            }
            return Err(e);
        }
        let count = n as usize / std::mem::size_of::<libc::input_event>();
        let mut out = Vec::with_capacity(count);
        for ev in &self.buf[..count] {
            let ty = ev.type_;
            if ty == EV_KEY || ty == EV_ABS {
                out.push(RawEvent::new(ty, ev.code, ev.value));
            }
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------------------------------
// Synthetic source: a scripted, in-memory stream (unit tests, no /dev/uinput needed).
// -------------------------------------------------------------------------------------------

/// A scripted event source for headless tests — no kernel, no `/dev/uinput`, CI-safe.
///
/// It is fed an `Identity`, a per-axis `AbsInfo` table (what `EVIOCGABS` would report), and a
/// queue of event BATCHES: each `poll()` returns the next batch (modelling events arriving over
/// time), or an empty vec once the queue drains (modelling an idle device).
#[derive(Default)]
pub struct ScriptedSource {
    ident: Identity,
    absinfo: std::collections::HashMap<u16, AbsInfo>,
    batches: std::collections::VecDeque<Vec<RawEvent>>,
}

impl ScriptedSource {
    pub fn new(ident: Identity) -> ScriptedSource {
        ScriptedSource { ident, ..Default::default() }
    }

    /// Register the declared range for an absolute axis (what `EVIOCGABS(code)` returns).
    pub fn with_abs(mut self, abs_code: u16, info: AbsInfo) -> ScriptedSource {
        self.absinfo.insert(abs_code, info);
        self
    }

    /// Queue one batch of events (one `poll()` worth).
    pub fn push_batch(&mut self, batch: Vec<RawEvent>) {
        self.batches.push_back(batch);
    }
}

impl EventSource for ScriptedSource {
    fn identity(&mut self) -> io::Result<Identity> {
        Ok(self.ident.clone())
    }

    fn absinfo(&mut self, abs_code: u16) -> io::Result<AbsInfo> {
        self.absinfo.get(&abs_code).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no scripted absinfo for abs code 0x{abs_code:x}"),
            )
        })
    }

    fn poll(&mut self, _timeout: Duration) -> io::Result<Vec<RawEvent>> {
        Ok(self.batches.pop_front().unwrap_or_default())
    }
}

// -------------------------------------------------------------------------------------------
// Multi-node source: route each control to the evdev node it lives on (tsp-bwrg.16).
// -------------------------------------------------------------------------------------------

/// A router over N per-node event sources for a MULTI-NODE device. A device like the a133 exposes
/// several evdev nodes (the `TRIMUI Player1` gamepad, the `sunxi-keyboard` LRADC where VOL± live,
/// the `audiocodec` Audio Jack); each control's descriptor `source` names its node. The engine
/// calls [`EventSource::set_active_source`] before pumping each control, and this router forwards
/// `poll`/`absinfo` to the matching sub-source.
///
/// `identity()` ALWAYS reports the PRIMARY node's identity (the device identity stamped onto the
/// emitted descriptor), independent of the active routing — routing changes which node we READ, not
/// who the device IS.
///
/// A control naming an UNKNOWN node routes to the primary and records the miss
/// ([`MultiSource::last_route_missed`]); it never fabricates events, so a control whose node is
/// unregistered simply captures nothing (the engine's existing NoActivity / optional-skip handling
/// applies). This makes single-vs-multi source a pure superset: a `MultiSource` with only the
/// primary registered behaves exactly like the single [`EvdevSource`] it wraps.
pub struct MultiSource {
    sources: std::collections::HashMap<String, Box<dyn EventSource>>,
    primary: String,
    active: String,
    last_route_missed: bool,
}

impl MultiSource {
    /// Build a router around the PRIMARY gamepad node (`primary` = its evdev name).
    pub fn new(primary: &str, primary_src: Box<dyn EventSource>) -> MultiSource {
        let mut sources = std::collections::HashMap::new();
        sources.insert(primary.to_string(), primary_src);
        MultiSource {
            sources,
            primary: primary.to_string(),
            active: primary.to_string(),
            last_route_missed: false,
        }
    }

    /// Register an additional node's source under its evdev NAME (e.g. `"sunxi-keyboard"`).
    pub fn add_source(&mut self, node: &str, src: Box<dyn EventSource>) {
        self.sources.insert(node.to_string(), src);
    }

    /// Whether the most recent [`set_active_source`](EventSource::set_active_source) named a node
    /// with no registered source (routed to primary as a fallback). Lets a caller surface a
    /// descriptor that references a node the run did not open, instead of silently misreading.
    pub fn last_route_missed(&self) -> bool {
        self.last_route_missed
    }

    fn active_mut(&mut self) -> &mut Box<dyn EventSource> {
        // `active` is only ever set to a registered key (set_active_source falls back to primary),
        // and primary is inserted at construction — so this key is always present.
        self.sources.get_mut(&self.active).expect("active source is always a registered node")
    }
}

impl EventSource for MultiSource {
    fn identity(&mut self) -> io::Result<Identity> {
        let p = self.primary.clone();
        self.sources.get_mut(&p).expect("primary source is registered at construction").identity()
    }

    fn absinfo(&mut self, abs_code: u16) -> io::Result<AbsInfo> {
        self.active_mut().absinfo(abs_code)
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<Vec<RawEvent>> {
        self.active_mut().poll(timeout)
    }

    fn set_active_source(&mut self, node: Option<&str>) {
        match node {
            None => {
                self.active = self.primary.clone();
                self.last_route_missed = false;
            }
            Some(n) if self.sources.contains_key(n) => {
                self.active = n.to_string();
                self.last_route_missed = false;
            }
            Some(_) => {
                // Unknown node: fall back to primary, record the miss (never fabricate).
                self.active = self.primary.clone();
                self.last_route_missed = true;
            }
        }
    }
}
