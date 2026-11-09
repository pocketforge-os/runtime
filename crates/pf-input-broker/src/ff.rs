//! App Pad rumble: SDL's `FF_RUMBLE` on the broker's App Pad reaches the `gpio-vibrator` actuator.
//!
//! * [`Forwarder`] maps the app's effect ids onto vibrator effect ids and enforces the safety
//!   stops (focus loss, safe-return, reader close, shutdown): `EV_FF id 0` plus erase.
//! * [`FfUinput`] / [`pump_once`] answer the uinput `UI_FF_UPLOAD` / `UI_FF_ERASE` handshake and
//!   forward `EV_FF` plays. The app's `EVIOCSFF` blocks in the kernel until that handshake is
//!   answered, so [`spawn_pump`] runs it on its own thread, apart from the input pump.
//! * [`VibratorSink`] is the real [`FfSink`] over the vibrator evdev fd (`EVIOCSFF`,
//!   `EVIOCRMFF`, `write(EV_FF)`), chosen by name + `FF_RUMBLE` ([`select_vibrator`]).
//!
//! Only `pf-input-broker` ever opens the vibrator node.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pocketforge::descriptor::Descriptor;

use crate::ioc;

pub use crate::ioc::FF_RUMBLE;

/// Bound on one effect's `replay.length` (ms).
pub const MAX_EFFECT_MS: u16 = 5000;

/// One rumble effect as the app uploaded it (magnitudes pass through unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfEffect {
    pub kind: u16,
    pub strong: u16,
    pub weak: u16,
    pub length_ms: u16,
}

/// The vibrator side. Real: [`VibratorSink`]. Tests: a fake.
pub trait FfSink {
    /// Upload (`vib < 0`: allocate) and return the vibrator's effect id.
    fn upload(&mut self, vib: i16, effect: &FfEffect) -> io::Result<i16>;
    /// `EV_FF vib value` (`value == 0` stops).
    fn play(&mut self, vib: i16, value: i32) -> io::Result<()>;
    fn erase(&mut self, vib: i16) -> io::Result<()>;
}

impl FfSink for Box<dyn FfSink + Send> {
    fn upload(&mut self, vib: i16, effect: &FfEffect) -> io::Result<i16> {
        (**self).upload(vib, effect)
    }
    fn play(&mut self, vib: i16, value: i32) -> io::Result<()> {
        (**self).play(vib, value)
    }
    fn erase(&mut self, vib: i16) -> io::Result<()> {
        (**self).erase(vib)
    }
}

fn errno(e: &io::Error) -> i32 {
    -e.raw_os_error().unwrap_or(libc::EIO)
}

/// Destination for `rumble: ...` journal lines. Production writes to stderr; tests capture.
pub trait RumbleLogSink: Send + Sync {
    fn line(&self, line: &str);
}

/// Journal sink (stderr, picked up by systemd).
pub struct StderrLog;

impl RumbleLogSink for StderrLog {
    fn line(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// Number of plays logged individually before only state changes and counters are logged.
pub const PLAY_LOG_FIRST: u64 = 20;
/// After the first [`PLAY_LOG_FIRST`] plays, an unchanged-state counter line every this many.
pub const PLAY_LOG_EVERY: u64 = 50;

/// App-id to vibrator-id map plus the safety stops.
pub struct Forwarder<S: FfSink> {
    sink: S,
    log: Arc<dyn RumbleLogSink>,
    plays: u64,
    last_play: Option<&'static str>,
    focused: bool,
    /// Effects as the app uploaded them (kept so a play after focus returns re-creates them).
    shadow: BTreeMap<i16, FfEffect>,
    /// app id -> vibrator id, for effects currently present on the vibrator.
    mapped: BTreeMap<i16, i16>,
}

impl<S: FfSink> Forwarder<S> {
    /// Starts unfocused: nothing reaches the vibrator until [`focus_gained`](Self::focus_gained).
    pub fn new(sink: S) -> Self {
        Self::with_log(sink, Arc::new(StderrLog))
    }

    pub fn with_log(sink: S, log: Arc<dyn RumbleLogSink>) -> Self {
        Self {
            sink,
            log,
            plays: 0,
            last_play: None,
            focused: false,
            shadow: BTreeMap::new(),
            mapped: BTreeMap::new(),
        }
    }

    pub fn focus_gained(&mut self) {
        self.focused = true;
    }

    fn log_play(&mut self, app_id: i16, value: i32, result: &'static str) {
        self.plays += 1;
        let changed = self.last_play != Some(result);
        self.last_play = Some(result);
        if changed || self.plays <= PLAY_LOG_FIRST {
            self.log.line(&format!(
                "rumble: ff_play app_id={app_id} value={value} result={result} count={}",
                self.plays
            ));
        } else if self.plays % PLAY_LOG_EVERY == 0 {
            self.log.line(&format!(
                "rumble: ff_play app_id={app_id} value={value} result={result} count={} bounded=1",
                self.plays
            ));
        }
    }

    fn log_upload(&self, app_id: i16, result: &str, focused: bool) {
        self.log.line(&format!(
            "rumble: ff_upload app_id={app_id} result={result} focused={focused}"
        ));
    }

    /// Handle one upload; returns the uinput `retval` (0 or `-errno`).
    pub fn upload(&mut self, app_id: i16, effect: &FfEffect) -> i32 {
        if effect.kind != FF_RUMBLE {
            self.log_upload(app_id, "err", self.focused);
            return -libc::EINVAL;
        }
        let mut effect = *effect;
        effect.length_ms = effect.length_ms.min(MAX_EFFECT_MS);
        self.shadow.insert(app_id, effect);
        if !self.focused {
            self.log_upload(app_id, "ok", false);
            return 0;
        }
        match self.push(app_id) {
            Ok(()) => {
                self.log_upload(app_id, "ok", true);
                0
            }
            Err(e) => {
                let no_act = matches!(e.raw_os_error(), Some(libc::ENODEV) | Some(libc::ENOENT))
                    || e.kind() == io::ErrorKind::NotFound;
                self.log_upload(app_id, if no_act { "no_actuator" } else { "err" }, true);
                errno(&e)
            }
        }
    }

    /// (Re)upload the retained effect to the vibrator, keeping an existing mapping.
    fn push(&mut self, app_id: i16) -> io::Result<()> {
        let Some(effect) = self.shadow.get(&app_id).copied() else {
            return Ok(());
        };
        let vib = self.mapped.get(&app_id).copied().unwrap_or(-1);
        let vib = self.sink.upload(vib, &effect)?;
        self.mapped.insert(app_id, vib);
        Ok(())
    }

    /// `EV_FF app_id value`. Plays are dropped without focus; `value == 0` always stops.
    pub fn play(&mut self, app_id: i16, value: i32) -> io::Result<()> {
        if value == 0 {
            self.log_play(app_id, 0, "stop");
            return match self.mapped.get(&app_id) {
                Some(&vib) => self.sink.play(vib, 0),
                None => Ok(()),
            };
        }
        if !self.focused {
            self.log_play(app_id, value, "dropped_unfocused");
            return Ok(());
        }
        if !self.shadow.contains_key(&app_id) {
            self.log_play(app_id, value, "dropped_no_effect");
            return Ok(());
        }
        if !self.mapped.contains_key(&app_id) {
            self.push(app_id)?;
        }
        self.log_play(app_id, value, "forwarded");
        self.sink.play(self.mapped[&app_id], value)
    }

    /// Handle one erase; returns the uinput `retval`.
    pub fn erase(&mut self, app_id: i16) -> i32 {
        self.shadow.remove(&app_id);
        match self.mapped.remove(&app_id) {
            Some(vib) => self.sink.erase(vib).map_or_else(|e| errno(&e), |()| 0),
            None => 0,
        }
    }

    /// `EV_FF id 0` + erase for every mapped effect. The first error is returned after all
    /// effects were attempted.
    fn stop_all(&mut self) -> io::Result<()> {
        let mut first = Ok(());
        for (_, vib) in std::mem::take(&mut self.mapped) {
            for r in [self.sink.play(vib, 0), self.sink.erase(vib)] {
                if let (Err(e), true) = (r, first.is_ok()) {
                    first = Err(e);
                }
            }
        }
        first
    }

    /// The App destination lost focus.
    pub fn focus_lost(&mut self) -> io::Result<()> {
        self.focused = false;
        self.stop_all()
    }

    /// Safe-return (system menu) entry.
    pub fn safe_return(&mut self) -> io::Result<()> {
        self.focused = false;
        self.stop_all()
    }

    /// The App Pad reader closed (the kernel flushed the app's effects).
    pub fn reader_closed(&mut self) -> io::Result<()> {
        self.focused = false;
        self.shadow.clear();
        self.stop_all()
    }

    /// Broker shutdown.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.reader_closed()
    }
}

/// Shared handle: the pump thread, the compositor focus handler, the safe-return gate and
/// shutdown all drive the same [`Forwarder`].
#[derive(Clone)]
pub struct RumbleControl(Arc<Mutex<Forwarder<Box<dyn FfSink + Send>>>>);

impl RumbleControl {
    pub fn new(sink: Box<dyn FfSink + Send>) -> Self {
        Self::with_log(sink, Arc::new(StderrLog))
    }

    pub fn with_log(sink: Box<dyn FfSink + Send>, log: Arc<dyn RumbleLogSink>) -> Self {
        Self(Arc::new(Mutex::new(Forwarder::with_log(sink, log))))
    }

    /// Upload through the shared forwarder (the pump's path, exposed for integration tests).
    #[doc(hidden)]
    pub fn upload_for_test(&self, app_id: i16, effect: &FfEffect) -> i32 {
        self.with(|f| f.upload(app_id, effect))
    }

    /// Play through the shared forwarder (see [`upload_for_test`](Self::upload_for_test)).
    #[doc(hidden)]
    pub fn play_for_test(&self, app_id: i16, value: i32) {
        Self::report("play", self.with(|f| f.play(app_id, value)));
    }

    /// Emit one `rumble: ...` line through this control's sink.
    pub fn log(&self, line: &str) {
        let sink = self.with(|f| f.log.clone());
        sink.line(line);
    }

    /// Log a compositor focus update (`surface` is a stable kind) and apply it.
    pub fn focus_update(&self, surface: &str, gained: bool) {
        let action = if gained { "gained" } else { "lost" };
        self.log(&format!("rumble: focus surface={surface} action={action}"));
        if gained {
            self.focus_gained();
        } else {
            self.focus_lost();
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Forwarder<Box<dyn FfSink + Send>>) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn report(what: &str, r: io::Result<()>) {
        if let Err(e) = r {
            eprintln!("pf-input-broker: rumble {what} failed: {e}");
        }
    }

    pub fn focus_gained(&self) {
        self.with(|f| f.focus_gained());
    }
    pub fn focus_lost(&self) {
        Self::report("focus_lost", self.with(|f| f.focus_lost()));
    }
    pub fn safe_return(&self) {
        Self::report("safe_return", self.with(|f| f.safe_return()));
    }
    pub fn reader_closed(&self) {
        Self::report("reader_closed", self.with(|f| f.reader_closed()));
    }
    pub fn shutdown(&self) {
        Self::report("shutdown", self.with(|f| f.shutdown()));
    }
}

/// One request read from the App Pad uinput fd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfEvent {
    /// `EV_UINPUT UI_FF_UPLOAD` with this request id.
    Upload(u32),
    /// `EV_UINPUT UI_FF_ERASE` with this request id.
    Erase(u32),
    /// `EV_FF code=id value`.
    Play { id: i16, value: i32 },
}

/// The App Pad uinput fd's FF handshake. Real: [`UinputFf`]. Tests: a fake.
pub trait FfUinput {
    /// Wait up to `timeout_ms` for the next request.
    fn next_event(&mut self, timeout_ms: i32) -> io::Result<Option<FfEvent>>;
    /// `UI_BEGIN_FF_UPLOAD`: the app's effect id and effect.
    fn begin_upload(&mut self, request_id: u32) -> io::Result<(i16, FfEffect)>;
    /// `UI_END_FF_UPLOAD` with `retval` (0 or `-errno`).
    fn end_upload(&mut self, request_id: u32, retval: i32) -> io::Result<()>;
    /// `UI_BEGIN_FF_ERASE`: the app's effect id.
    fn begin_erase(&mut self, request_id: u32) -> io::Result<i16>;
    fn end_erase(&mut self, request_id: u32, retval: i32) -> io::Result<()>;
}

/// Answer one already-read request. Every begun upload/erase is always ended, or the app's
/// `EVIOCSFF` would block forever.
pub fn dispatch<U: FfUinput, S: FfSink>(
    uinput: &mut U,
    fwd: &mut Forwarder<S>,
    event: FfEvent,
) -> io::Result<()> {
    match event {
        FfEvent::Upload(rid) => {
            let retval = match uinput.begin_upload(rid) {
                Ok((id, effect)) => fwd.upload(id, &effect),
                Err(e) => errno(&e),
            };
            uinput.end_upload(rid, retval)
        }
        FfEvent::Erase(rid) => {
            let retval = match uinput.begin_erase(rid) {
                Ok(id) => fwd.erase(id),
                Err(e) => errno(&e),
            };
            uinput.end_erase(rid, retval)
        }
        FfEvent::Play { id, value } => fwd.play(id, value),
    }
}

/// Read and answer one request. `Ok(false)` when none arrived within `timeout_ms`.
pub fn pump_once<U: FfUinput, S: FfSink>(
    uinput: &mut U,
    fwd: &mut Forwarder<S>,
    timeout_ms: i32,
) -> io::Result<bool> {
    match uinput.next_event(timeout_ms)? {
        Some(event) => dispatch(uinput, fwd, event).map(|()| true),
        None => Ok(false),
    }
}

/// Run until `stop`. A failed vibrator write is logged and never ends the loop; a failed uinput
/// read does.
pub fn run_pump<U: FfUinput>(
    uinput: &mut U,
    control: &RumbleControl,
    stop: &AtomicBool,
) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        if let Some(event) = uinput.next_event(100)? {
            if let Err(e) = control.with(|f| dispatch(uinput, f, event)) {
                eprintln!("pf-input-broker: rumble request failed: {e}");
            }
        }
    }
    Ok(())
}

/// Run [`run_pump`] on its own thread.
pub fn spawn_pump<U: FfUinput + Send + 'static>(
    mut uinput: U,
    control: RumbleControl,
    stop: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(e) = run_pump(&mut uinput, &control, stop) {
            eprintln!("pf-input-broker: rumble pump ended: {e}");
        }
    })
}

/// A vibrator candidate, as seen on an input node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibratorCandidate {
    pub path: String,
    pub name: String,
    pub has_ff_rumble: bool,
}

/// Pick the actuator by input name AND `FF_RUMBLE`, never by event number.
pub fn select_vibrator<'a>(
    candidates: &'a [VibratorCandidate],
    name: &str,
) -> Option<&'a VibratorCandidate> {
    candidates
        .iter()
        .find(|c| c.name == name && c.has_ff_rumble)
}

/// The vibrator input name the descriptor's `kind = "rumble"` actuator declares (`source`).
pub fn vibrator_source(descriptor: &Descriptor) -> Option<String> {
    descriptor
        .actuators
        .iter()
        .find(|a| a.kind.eq_ignore_ascii_case("rumble"))
        .and_then(|a| a.source.clone())
}

// --- real devices -------------------------------------------------------------------------

/// `EV_UINPUT`: uinput's own request channel on the uinput fd.
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;

fn poll_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        };
    }
    Ok(n > 0 && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is a valid exclusive pointer to the struct `req` is sized for.
    if unsafe { libc::ioctl(fd, req as libc::Ioctl, arg as *mut T) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn effect_from_kernel(e: &libc::ff_effect) -> (i16, FfEffect) {
    // ff_rumble_effect {strong, weak} is the first member of the union.
    let u = e.u[0];
    (
        e.id,
        FfEffect {
            kind: e.type_,
            strong: (u & 0xffff) as u16,
            weak: ((u >> 16) & 0xffff) as u16,
            length_ms: e.replay.length,
        },
    )
}

/// The real [`FfUinput`] over (a dup of) the App Pad uinput fd.
pub struct UinputFf {
    fd: OwnedFd,
    queue: VecDeque<FfEvent>,
}

impl UinputFf {
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            queue: VecDeque::new(),
        }
    }
}

impl FfUinput for UinputFf {
    fn next_event(&mut self, timeout_ms: i32) -> io::Result<Option<FfEvent>> {
        if let Some(e) = self.queue.pop_front() {
            return Ok(Some(e));
        }
        if !poll_readable(self.fd.as_raw_fd(), timeout_ms)? {
            return Ok(None);
        }
        let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
        // SAFETY: reading one input_event into a valid struct.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                &mut ev as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::input_event>(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(e),
            };
        }
        if n as usize != std::mem::size_of::<libc::input_event>() {
            return Ok(None);
        }
        Ok(match (ev.type_, ev.code) {
            (EV_UINPUT, UI_FF_UPLOAD) => Some(FfEvent::Upload(ev.value as u32)),
            (EV_UINPUT, UI_FF_ERASE) => Some(FfEvent::Erase(ev.value as u32)),
            (ioc::EV_FF, id) if id != 0x60 && id != 0x61 => Some(FfEvent::Play {
                id: id as i16,
                value: ev.value,
            }),
            _ => None, // FF_GAIN / FF_AUTOCENTER and anything else: not forwarded
        })
    }

    fn begin_upload(&mut self, request_id: u32) -> io::Result<(i16, FfEffect)> {
        let mut up: libc::uinput_ff_upload = unsafe { std::mem::zeroed() };
        up.request_id = request_id;
        ioctl_ptr(self.fd.as_raw_fd(), ioc::ui_begin_ff_upload(), &mut up)?;
        Ok(effect_from_kernel(&up.effect))
    }

    fn end_upload(&mut self, request_id: u32, retval: i32) -> io::Result<()> {
        let mut up: libc::uinput_ff_upload = unsafe { std::mem::zeroed() };
        up.request_id = request_id;
        up.retval = retval;
        ioctl_ptr(self.fd.as_raw_fd(), ioc::ui_end_ff_upload(), &mut up)
    }

    fn begin_erase(&mut self, request_id: u32) -> io::Result<i16> {
        let mut er: libc::uinput_ff_erase = unsafe { std::mem::zeroed() };
        er.request_id = request_id;
        ioctl_ptr(self.fd.as_raw_fd(), ioc::ui_begin_ff_erase(), &mut er)?;
        Ok(er.effect_id as i16)
    }

    fn end_erase(&mut self, request_id: u32, retval: i32) -> io::Result<()> {
        let mut er: libc::uinput_ff_erase = unsafe { std::mem::zeroed() };
        er.request_id = request_id;
        er.retval = retval;
        ioctl_ptr(self.fd.as_raw_fd(), ioc::ui_end_ff_erase(), &mut er)
    }
}

/// The real [`FfSink`]: the vibrator's evdev fd.
pub struct VibratorSink {
    fd: OwnedFd,
}

impl VibratorSink {
    /// Find the vibrator by input name + `FF_RUMBLE` (never by event number) and open it.
    pub fn open_by_identity(name: &str) -> io::Result<Self> {
        let mut candidates = Vec::new();
        for ent in std::fs::read_dir("/dev/input")?.flatten() {
            let file = ent.file_name();
            if !file.to_string_lossy().starts_with("event") {
                continue;
            }
            let path = ent.path();
            let Ok(fd) = open_rdwr(&path.to_string_lossy()) else {
                continue;
            };
            let mut buf = [0u8; 128];
            // SAFETY: buf is writable for the length passed.
            let n = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    ioc::eviocgname(buf.len()) as libc::Ioctl,
                    buf.as_mut_ptr(),
                )
            };
            if n < 0 {
                continue;
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let mut bits = [0u8; 16];
            // SAFETY: bits is writable for the length passed.
            let r = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    ioc::eviocgbit(ioc::EV_FF, bits.len()) as libc::Ioctl,
                    bits.as_mut_ptr(),
                )
            };
            let rumble = FF_RUMBLE as usize;
            candidates.push(VibratorCandidate {
                path: path.to_string_lossy().into_owned(),
                name: String::from_utf8_lossy(&buf[..end]).into_owned(),
                has_ff_rumble: r >= 0 && bits[rumble / 8] & (1 << (rumble % 8)) != 0,
            });
        }
        let chosen = select_vibrator(&candidates, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no FF_RUMBLE input named {name}"),
            )
        })?;
        Ok(Self {
            fd: open_rdwr(&chosen.path)?,
        })
    }
}

fn open_rdwr(path: &str) -> io::Result<OwnedFd> {
    let c =
        std::ffi::CString::new(path).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: valid C string.
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

impl FfSink for VibratorSink {
    fn upload(&mut self, vib: i16, effect: &FfEffect) -> io::Result<i16> {
        let mut e: libc::ff_effect = unsafe { std::mem::zeroed() };
        e.type_ = FF_RUMBLE;
        e.id = vib;
        e.replay.length = effect.length_ms;
        e.u[0] = effect.strong as u64 | (effect.weak as u64) << 16;
        ioctl_ptr(self.fd.as_raw_fd(), ioc::eviocsff(), &mut e)?;
        Ok(e.id)
    }

    fn play(&mut self, vib: i16, value: i32) -> io::Result<()> {
        let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
        ev.type_ = ioc::EV_FF;
        ev.code = vib as u16;
        ev.value = value;
        let size = std::mem::size_of::<libc::input_event>();
        // SAFETY: writing one valid input_event.
        let n = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &ev as *const _ as *const libc::c_void,
                size,
            )
        };
        if n == size as isize {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn erase(&mut self, vib: i16) -> io::Result<()> {
        // SAFETY: EVIOCRMFF takes the effect id by value.
        if unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioc::eviocrmff() as libc::Ioctl,
                vib as libc::c_int,
            )
        } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    const EINVAL: i32 = -libc::EINVAL;

    #[derive(Clone, Default)]
    struct Log(Rc<RefCell<Vec<String>>>);
    struct Fake(Log, i16);
    impl FfSink for Fake {
        fn upload(&mut self, vib: i16, e: &FfEffect) -> std::io::Result<i16> {
            let id = if vib < 0 {
                self.1 += 1;
                self.1
            } else {
                vib
            };
            (self.0).0.borrow_mut().push(format!(
                "up {id} {:#x} {:#x} {}",
                e.strong, e.weak, e.length_ms
            ));
            Ok(id)
        }
        fn play(&mut self, vib: i16, value: i32) -> std::io::Result<()> {
            (self.0).0.borrow_mut().push(format!("play {vib} {value}"));
            Ok(())
        }
        fn erase(&mut self, vib: i16) -> std::io::Result<()> {
            (self.0).0.borrow_mut().push(format!("erase {vib}"));
            Ok(())
        }
    }

    fn rumble(len: u16) -> FfEffect {
        FfEffect {
            kind: FF_RUMBLE,
            strong: 0x8000,
            weak: 0x2000,
            length_ms: len,
        }
    }

    fn fwd() -> (Forwarder<Fake>, Log) {
        let log = Log::default();
        let mut f = Forwarder::new(Fake(log.clone(), 40));
        f.focus_gained();
        (f, log)
    }

    #[test]
    fn forwards_upload_play_erase_to_vibrator() {
        let (mut f, log) = fwd();
        assert_eq!(f.upload(3, &rumble(200)), 0);
        f.play(3, 1).unwrap();
        assert_eq!(f.erase(3), 0);
        assert_eq!(
            *log.0.borrow(),
            ["up 41 0x8000 0x2000 200", "play 41 1", "erase 41"]
        );

        // Non-RUMBLE is refused without touching the vibrator; long effects are clamped.
        log.0.borrow_mut().clear();
        let periodic = FfEffect {
            kind: 0x51,
            ..rumble(10)
        };
        assert_eq!(f.upload(4, &periodic), EINVAL);
        assert!(log.0.borrow().is_empty());
        assert_eq!(f.upload(5, &rumble(9000)), 0);
        assert_eq!(*log.0.borrow(), ["up 42 0x8000 0x2000 5000"]);
    }

    #[test]
    fn stops_on_focus_loss_close_and_safe_return() {
        for ev in ["focus", "close", "safe", "shutdown"] {
            let (mut f, log) = fwd();
            assert_eq!(f.upload(1, &rumble(100)), 0);
            f.play(1, 1).unwrap();
            log.0.borrow_mut().clear();
            match ev {
                "focus" => f.focus_lost(),
                "close" => f.reader_closed(),
                "safe" => f.safe_return(),
                _ => f.shutdown(),
            }
            .unwrap();
            assert_eq!(*log.0.borrow(), ["play 41 0", "erase 41"], "{ev}");
            // Nothing plays without focus.
            log.0.borrow_mut().clear();
            f.play(1, 1).unwrap();
            assert!(log.0.borrow().is_empty(), "{ev}");
        }
    }

    #[test]
    fn focus_regained_replays_from_the_retained_effect() {
        let (mut f, log) = fwd();
        f.upload(1, &rumble(100));
        f.focus_lost().unwrap();
        f.focus_gained();
        log.0.borrow_mut().clear();
        f.play(1, 1).unwrap();
        assert_eq!(*log.0.borrow(), ["up 42 0x8000 0x2000 100", "play 42 1"]);
    }

    #[test]
    fn vibrator_selected_by_identity() {
        let c = |p: &str, n: &str, ff: bool| VibratorCandidate {
            path: p.into(),
            name: n.into(),
            has_ff_rumble: ff,
        };
        let a = [
            c("/dev/input/event0", "gpio-vibrator", false), // lookalike without FF
            c("/dev/input/event1", "other", true),
            c("/dev/input/event9", "gpio-vibrator", true),
        ];
        assert_eq!(
            select_vibrator(&a, "gpio-vibrator").unwrap().path,
            "/dev/input/event9"
        );
        let b = [a[2].clone(), a[0].clone()];
        assert_eq!(
            select_vibrator(&b, "gpio-vibrator").unwrap().path,
            "/dev/input/event9"
        );
        assert!(select_vibrator(&a[..2], "gpio-vibrator").is_none());
    }

    struct FakeUinput {
        events: VecDeque<FfEvent>,
        uploads: Vec<(u32, (i16, FfEffect))>,
        erases: Vec<(u32, i16)>,
        ended: Vec<String>,
    }
    impl FfUinput for FakeUinput {
        fn next_event(&mut self, _timeout_ms: i32) -> std::io::Result<Option<FfEvent>> {
            Ok(self.events.pop_front())
        }
        fn begin_upload(&mut self, rid: u32) -> std::io::Result<(i16, FfEffect)> {
            Ok(self.uploads.iter().find(|u| u.0 == rid).unwrap().1)
        }
        fn end_upload(&mut self, rid: u32, retval: i32) -> std::io::Result<()> {
            self.ended.push(format!("up {rid} {retval}"));
            Ok(())
        }
        fn begin_erase(&mut self, rid: u32) -> std::io::Result<i16> {
            Ok(self.erases.iter().find(|u| u.0 == rid).unwrap().1)
        }
        fn end_erase(&mut self, rid: u32, retval: i32) -> std::io::Result<()> {
            self.ended.push(format!("er {rid} {retval}"));
            Ok(())
        }
    }

    #[test]
    fn pump_answers_upload_erase_requests_and_drives_play() {
        let (mut f, log) = fwd();
        let mut u = FakeUinput {
            events: VecDeque::from([
                FfEvent::Upload(7),
                FfEvent::Play { id: 2, value: 1 },
                FfEvent::Upload(8),
                FfEvent::Erase(9),
            ]),
            uploads: vec![
                (7, (2, rumble(50))),
                (
                    8,
                    (
                        3,
                        FfEffect {
                            kind: 0x52,
                            ..rumble(5)
                        },
                    ),
                ),
            ],
            erases: vec![(9, 2)],
            ended: vec![],
        };
        while pump_once(&mut u, &mut f, 0).unwrap() {}
        assert_eq!(
            u.ended,
            ["up 7 0", format!("up 8 {EINVAL}").as_str(), "er 9 0"]
        );
        assert_eq!(
            *log.0.borrow(),
            ["up 41 0x8000 0x2000 50", "play 41 1", "erase 41"]
        );
    }

    #[test]
    fn rumble_actuator_is_read_from_the_descriptor() {
        let d = |extra: &str| {
            pocketforge::descriptor::Descriptor::from_toml(&format!(
                "[identity]\nid=\"x\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n\
                 [[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n{extra}"
            ))
            .unwrap()
        };
        let with = d("[[actuators]]\nid=\"rumble\"\nkind=\"rumble\"\nev_type=\"EV_FF\"\ncode=\"FF_RUMBLE\"\nsource=\"gpio-vibrator\"\n");
        let led = d("[[actuators]]\nid=\"leds\"\nkind=\"led_array\"\ncount=3\n");
        assert_eq!(vibrator_source(&with).as_deref(), Some("gpio-vibrator"));
        assert_eq!(vibrator_source(&led), None);
        assert_eq!(vibrator_source(&d("")), None);
    }

    #[test]
    fn threaded_pump_answers_while_the_input_side_is_busy_and_control_stops() {
        use std::sync::mpsc;
        struct Chan(mpsc::Receiver<FfEvent>, mpsc::Sender<String>);
        impl FfUinput for Chan {
            fn next_event(&mut self, ms: i32) -> std::io::Result<Option<FfEvent>> {
                Ok(self
                    .0
                    .recv_timeout(std::time::Duration::from_millis(ms as u64))
                    .ok())
            }
            fn begin_upload(&mut self, _: u32) -> std::io::Result<(i16, FfEffect)> {
                Ok((1, rumble(10)))
            }
            fn end_upload(&mut self, r: u32, v: i32) -> std::io::Result<()> {
                self.1.send(format!("end {r} {v}")).unwrap();
                Ok(())
            }
            fn begin_erase(&mut self, _: u32) -> std::io::Result<i16> {
                Ok(1)
            }
            fn end_erase(&mut self, _: u32, _: i32) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Sink(mpsc::Sender<String>);
        impl FfSink for Sink {
            fn upload(&mut self, _: i16, _: &FfEffect) -> std::io::Result<i16> {
                Ok(7)
            }
            fn play(&mut self, v: i16, n: i32) -> std::io::Result<()> {
                self.0.send(format!("play {v} {n}")).unwrap();
                Ok(())
            }
            fn erase(&mut self, v: i16) -> std::io::Result<()> {
                self.0.send(format!("erase {v}")).unwrap();
                Ok(())
            }
        }
        static STOP: AtomicBool = AtomicBool::new(false);
        let (etx, erx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        let control = RumbleControl::new(Box::new(Sink(otx.clone())));
        control.focus_gained();
        let handle = spawn_pump(Chan(erx, otx), control.clone(), &STOP);
        let t = std::time::Duration::from_secs(5);
        etx.send(FfEvent::Upload(4)).unwrap();
        assert_eq!(orx.recv_timeout(t).unwrap(), "end 4 0");
        etx.send(FfEvent::Play { id: 1, value: 1 }).unwrap();
        assert_eq!(orx.recv_timeout(t).unwrap(), "play 7 1");
        control.safe_return();
        assert_eq!(orx.recv_timeout(t).unwrap(), "play 7 0");
        assert_eq!(orx.recv_timeout(t).unwrap(), "erase 7");
        STOP.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn ff_ioctl_numbers_match_the_kernel_abi() {
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(ioc::eviocsff(), 0x4030_4580);
            assert_eq!(ioc::eviocrmff(), 0x4004_4581);
            assert_eq!(ioc::ui_begin_ff_upload(), 0xc068_55c8);
            assert_eq!(ioc::ui_end_ff_upload(), 0x4068_55c9);
            assert_eq!(ioc::ui_begin_ff_erase(), 0xc00c_55ca);
            assert_eq!(ioc::ui_end_ff_erase(), 0x400c_55cb);
        }
    }
}

#[cfg(test)]
mod rumble_log {
    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl RumbleLogSink for Capture {
        fn line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_string());
        }
    }
    impl Capture {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn with(&self, prefix: &str) -> Vec<String> {
            self.lines()
                .into_iter()
                .filter(|l| l.starts_with(prefix))
                .collect()
        }
    }

    struct Sink {
        fail_upload: Option<i32>,
    }
    impl FfSink for Sink {
        fn upload(&mut self, vib: i16, _: &FfEffect) -> io::Result<i16> {
            match self.fail_upload {
                Some(e) => Err(io::Error::from_raw_os_error(e)),
                None => Ok(if vib < 0 { 7 } else { vib }),
            }
        }
        fn play(&mut self, _: i16, _: i32) -> io::Result<()> {
            Ok(())
        }
        fn erase(&mut self, _: i16) -> io::Result<()> {
            Ok(())
        }
    }

    fn eff() -> FfEffect {
        FfEffect {
            kind: FF_RUMBLE,
            strong: 0x8000,
            weak: 0,
            length_ms: 100,
        }
    }

    fn fwd(fail: Option<i32>) -> (Forwarder<Sink>, Capture) {
        let cap = Capture::default();
        (
            Forwarder::with_log(Sink { fail_upload: fail }, Arc::new(cap.clone())),
            cap,
        )
    }

    #[test]
    fn upload_ok_and_err_are_logged_with_app_id() {
        let (mut f, cap) = fwd(None);
        f.focus_gained();
        assert_eq!(f.upload(3, &eff()), 0);
        let bad = FfEffect {
            kind: 0x52,
            ..eff()
        };
        assert!(f.upload(4, &bad) < 0);
        assert_eq!(
            cap.with("rumble: ff_upload"),
            vec![
                "rumble: ff_upload app_id=3 result=ok focused=true",
                "rumble: ff_upload app_id=4 result=err focused=true"
            ]
        );
    }

    #[test]
    fn upload_without_actuator_is_distinguished() {
        let (mut f, cap) = fwd(Some(libc::ENODEV));
        f.focus_gained();
        assert!(f.upload(1, &eff()) < 0);
        assert_eq!(
            cap.with("rumble: ff_upload"),
            vec!["rumble: ff_upload app_id=1 result=no_actuator focused=true"]
        );
    }

    #[test]
    fn unfocused_play_logs_dropped_unfocused() {
        let (mut f, cap) = fwd(None);
        f.upload(1, &eff());
        f.play(1, 1).unwrap();
        let p = cap.with("rumble: ff_play");
        assert_eq!(p.len(), 1);
        assert!(
            p[0].starts_with("rumble: ff_play app_id=1 value=1 result=dropped_unfocused"),
            "{p:?}"
        );
    }

    #[test]
    fn focused_play_logs_forwarded_and_stop() {
        let (mut f, cap) = fwd(None);
        f.focus_gained();
        f.upload(1, &eff());
        f.play(1, 1).unwrap();
        f.play(1, 0).unwrap();
        let p = cap.with("rumble: ff_play");
        assert!(p[0].contains("result=forwarded"), "{p:?}");
        assert!(p[1].contains("value=0 result=stop"), "{p:?}");
    }

    #[test]
    fn play_without_uploaded_effect_logs_dropped_no_effect() {
        let (mut f, cap) = fwd(None);
        f.focus_gained();
        f.play(9, 1).unwrap();
        assert!(
            cap.with("rumble: ff_play")[0].contains("app_id=9 value=1 result=dropped_no_effect")
        );
    }

    #[test]
    fn play_logging_is_bounded_first_20_then_every_50() {
        let (mut f, cap) = fwd(None);
        f.focus_gained();
        f.upload(1, &eff());
        for _ in 0..200 {
            f.play(1, 1).unwrap();
        }
        let p = cap.with("rumble: ff_play");
        // plays 1..=20, then counters at 50, 100, 150, 200.
        assert_eq!(p.len(), 20 + 4, "{p:?}");
        assert!(p[20].contains("count=50"), "{}", p[20]);
        assert!(p[23].contains("count=200"));
    }

    #[test]
    fn play_state_change_is_always_logged_even_after_budget() {
        let (mut f, cap) = fwd(None);
        f.focus_gained();
        f.upload(1, &eff());
        for _ in 0..30 {
            f.play(1, 1).unwrap();
        }
        let before = cap.with("rumble: ff_play").len();
        f.focus_lost().unwrap();
        f.play(1, 1).unwrap();
        let p = cap.with("rumble: ff_play");
        assert_eq!(p.len(), before + 1);
        assert!(p.last().unwrap().contains("result=dropped_unfocused"));
    }

    #[test]
    fn control_focus_update_logs_surface_and_action() {
        let cap = Capture::default();
        let c =
            RumbleControl::with_log(Box::new(Sink { fail_upload: None }), Arc::new(cap.clone()));
        c.focus_update("app", true);
        c.focus_update("trusted", false);
        assert_eq!(
            cap.with("rumble: focus"),
            vec![
                "rumble: focus surface=app action=gained",
                "rumble: focus surface=trusted action=lost"
            ]
        );
    }
}
