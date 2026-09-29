//! The **input broker** — the load-bearing v0 enforcement: open the real evdev source,
//! `EVIOCGRAB` it (exclusive), and pump its events through the descriptor remap + the rate-limit
//! policy into a uinput re-emit device the app reads. The app gets the re-emit read fd via
//! `Acquire("input")` over a Unix socket (`SCM_RIGHTS`); it can no longer reach the real node.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use pf_wire::{recv_request, send_response, Op, Request, Response, Status};
use pocketforge::descriptor::Descriptor;
use pocketforge::server::handle_request;
use pocketforge::Backend;

use crate::evdev::Evdev;
use crate::ioc;
use crate::policy::TokenBucket;
use crate::remap::{AbsAction, Remap};
use crate::safe_return::{SafeReturnGate, SafeReturnIntake, GUIDE_CODE};
use crate::scm;
use crate::uinput::Uinput;

const MAX_REPORT_EVENTS: usize = 256;
const ACQUIRE_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// A source that can answer the `EVIOCGBIT` question the broker's capability check asks: does it
/// advertise every one of `codes` for `event_type`? [`Evdev`] answers from the opened fd; a
/// hermetic test answers from a device's uinput setup (`tsp-f3fm.217`), so the check the daemon
/// runs and the check the contract test runs are one function, [`missing_source_capabilities`].
pub trait SourceCapabilities {
    /// Whether every code in `codes` is advertised for `event_type` (`EV_KEY` / `EV_ABS`).
    fn supports(&self, event_type: u16, codes: &[u16]) -> io::Result<bool>;
}

impl SourceCapabilities for Evdev {
    fn supports(&self, event_type: u16, codes: &[u16]) -> io::Result<bool> {
        Evdev::supports(self, event_type, codes)
    }
}

/// The descriptor-required source codes a source does NOT advertise. Empty means the source
/// satisfies the descriptor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MissingCapabilities {
    /// Required `EV_KEY` codes the source lacks.
    pub keys: Vec<u16>,
    /// Required `EV_ABS` codes the source lacks.
    pub abs: Vec<u16>,
}

impl MissingCapabilities {
    /// True when nothing required is missing.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.abs.is_empty()
    }
}

impl std::fmt::Display for MissingCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex = |v: &[u16]| {
            v.iter()
                .map(|c| format!("{c:#x}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        write!(
            f,
            "EV_KEY [{}] EV_ABS [{}]",
            hex(&self.keys),
            hex(&self.abs)
        )
    }
}

/// The broker's source capability check: every `EV_KEY`/`EV_ABS` code the descriptor's
/// non-system rows require ([`Remap::required_source_keys`] / [`Remap::required_source_abs`]),
/// minus what `source` advertises. `class = "system"` rows (VOL±, read from their own `source`
/// node) never enter the required set, so they can never make a pad source fail.
pub fn missing_source_capabilities(
    source: &impl SourceCapabilities,
    remap: &Remap,
) -> io::Result<MissingCapabilities> {
    let mut missing = MissingCapabilities::default();
    for &code in remap.required_source_keys() {
        if !source.supports(ioc::EV_KEY, &[code])? {
            missing.keys.push(code);
        }
    }
    for &code in remap.required_source_abs() {
        if !source.supports(ioc::EV_ABS, &[code])? {
            missing.abs.push(code);
        }
    }
    Ok(missing)
}

fn wire_err(e: pf_wire::WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Read events from a raw evdev fd (non-blocking; 0 on `EAGAIN`). Used to read the handed/shared
/// app fd and to prove the grabbed source is silent.
pub fn read_events_raw(fd: RawFd, out: &mut [libc::input_event]) -> io::Result<usize> {
    let cap = std::mem::size_of_val(out);
    // SAFETY: out is a valid buffer of `cap` bytes.
    let n = unsafe { libc::read(fd, out.as_mut_ptr() as *mut libc::c_void, cap) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize / std::mem::size_of::<libc::input_event>())
}

/// The grabbed source + re-emit sink + remap/policy. Owns the live devices; the grab is released
/// on drop.
pub struct InputBroker {
    source: Evdev,
    sink: Uinput,
    start: std::time::Instant,
    pump: ReportPump,
}

/// Authoritative source state the pump consults after `SYN_DROPPED`: the grabbed evdev source in
/// production, a fixture in hermetic tests.
pub(crate) trait SourceState {
    fn pressed_keys(&self, codes: &[u16]) -> io::Result<Vec<u16>>;
    fn abs_value(&self, code: u16) -> io::Result<i32>;
}

impl SourceState for Evdev {
    fn pressed_keys(&self, codes: &[u16]) -> io::Result<Vec<u16>> {
        Evdev::pressed_keys(self, codes)
    }
    fn abs_value(&self, code: u16) -> io::Result<i32> {
        Evdev::abs_value(self, code)
    }
}

/// The device-free report pipeline: descriptor remap, rate-limit policy, report framing,
/// `SYN_DROPPED` resync and (with `--safe-return-sock`) the protected guide gate.
pub(crate) struct ReportPump {
    remap: Remap,
    bucket: TokenBucket,
    pending_report: Vec<(u16, u16, i32)>,
    pending_report_oversized: bool,
    resynchronizing: bool,
    pressed: HashSet<u16>,
    abs_state: HashMap<u16, i32>,
    /// `None` (no `--safe-return-sock`) leaves the stream exactly as before: guide passes through.
    safe_return: Option<SafeReturnGate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedIdentity {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl ExpectedIdentity {
    pub fn from_descriptor(descriptor: &Descriptor) -> io::Result<Self> {
        let m = descriptor.identity.r#match.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "descriptor identity.match is required",
            )
        })?;
        let hex = |s: &str| {
            u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid identity hex {s:?}"),
                )
            })
        };
        let word = |offset: usize| -> io::Result<u16> {
            let guid = descriptor.identity.sdl_guid.as_bytes();
            if guid.len() != 32 || !guid.is_ascii() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid SDL GUID",
                ));
            }
            let byte = |at: usize| {
                std::str::from_utf8(&guid[at..at + 2])
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))
            };
            let lo = byte(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))?;
            let hi = byte(offset + 2)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))?;
            Ok(lo as u16 | (hi as u16) << 8)
        };
        Ok(Self {
            name: m.evdev_name.clone(),
            bus: word(0)?,
            vendor: hex(&m.vid)?,
            product: hex(&m.pid)?,
            version: word(24)?,
        })
    }

    pub fn matches(&self, name: &str, id: (u16, u16, u16, u16)) -> bool {
        name == self.name
            && id.0 == self.bus
            && id.1 == self.vendor
            && id.2 == self.product
            && id.3 == self.version
            && !name.starts_with("PocketForge Input (")
    }
}

impl InputBroker {
    /// Open `source_path`, grab it (the enforcing default), and stand up the descriptor-derived
    /// re-emit device.
    pub fn start(
        source_path: impl AsRef<Path>,
        descriptor: &Descriptor,
    ) -> io::Result<InputBroker> {
        InputBroker::start_with(source_path, descriptor, true)
    }

    /// As [`start`](Self::start), but `grab=false` is the R-C **blessed-binary** path (Steam Link):
    /// re-emit + hand a fd WITHOUT the exclusive grab, so a consumer that is itself a `uinput`
    /// producer is not broken. The re-emit device still normalizes codes; it just is not exclusive.
    pub fn start_with(
        source_path: impl AsRef<Path>,
        descriptor: &Descriptor,
        grab: bool,
    ) -> io::Result<InputBroker> {
        let remap = Remap::from_descriptor(descriptor)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let (source, sink) = acquire_then_create(
            || {
                let mut source = Evdev::open(source_path)?;
                Self::validate_source(&source, descriptor, &remap)?;
                if grab {
                    source.grab()?;
                }
                Ok(source)
            },
            |_| Uinput::create(remap.spec()),
        )?;
        Ok(InputBroker {
            source,
            sink,
            start: std::time::Instant::now(),
            pump: ReportPump::new(remap),
        })
    }

    /// Make this broker the protected SafeReturn intake (`--safe-return-sock`): guide/`BTN_MODE`
    /// press and release are dropped from the re-emit stream, and each press edge asks
    /// `intake` to send one SafeReturn to the session authority.
    pub fn with_safe_return(mut self, intake: SafeReturnIntake) -> InputBroker {
        self.pump.safe_return = Some(SafeReturnGate::new(intake));
        self
    }

    fn validate_source(source: &Evdev, descriptor: &Descriptor, remap: &Remap) -> io::Result<()> {
        let expected = ExpectedIdentity::from_descriptor(descriptor)?;
        if !expected.matches(&source.name()?, source.id()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened source identity mismatch",
            ));
        }
        let missing = missing_source_capabilities(source, remap)?;
        if !missing.is_empty() {
            // The prefix is kept verbatim (journal greps key on it); the suffix names the codes,
            // which the tsp-f3fm.215 bench had to reconstruct from bitmaps.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("opened source lacks descriptor-required capabilities: missing {missing}"),
            ));
        }
        Ok(())
    }

    /// Discover exactly one descriptor-matching event node. Every candidate is identified from
    /// its opened fd; `start` opens and validates the winner again before grabbing it.
    pub fn discover(descriptor: &Descriptor) -> io::Result<PathBuf> {
        let expected = ExpectedIdentity::from_descriptor(descriptor)?;
        let remap = Remap::from_descriptor(descriptor)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir("/dev/input")? {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
            {
                continue;
            }
            candidates.push(path);
        }
        discover_candidates(candidates, |path| {
            let dev = Evdev::open(path)?;
            Ok(expected.matches(&dev.name()?, dev.id()?)
                && missing_source_capabilities(&dev, &remap)?.is_empty())
        })
    }

    fn resolve_matches(mut matches: Vec<PathBuf>) -> io::Result<PathBuf> {
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no input device matches descriptor identity and capabilities",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("multiple input devices match descriptor: {matches:?}"),
            )),
        }
    }

    /// The re-emit `/dev/input/eventN` node path (what the app reads / the fd handed over points at).
    pub fn node_path(&self) -> Option<String> {
        self.sink.node().map(|s| s.to_string())
    }

    /// The grabbed source device's name (for logging / the blessed-binary check).
    pub fn source_name(&self) -> io::Result<String> {
        self.source.name()
    }

    /// Drain pending source events through remap + policy into the sink. Returns events emitted.
    pub fn pump_once(&mut self) -> io::Result<usize> {
        let mut buf: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
        let n = self.source.read_events(&mut buf)?;
        let now = self.start.elapsed().as_secs_f64();
        let sink = &self.sink;
        self.pump
            .process(&buf[..n], now, &self.source, &mut |ty, code, value| {
                sink.emit(ty, code, value)
            })
    }

    /// Block up to `timeout_ms` for the source to become readable. `true` if events are pending.
    pub fn wait_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.source.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(e);
        }
        if rc > 0 && (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "upstream input device disconnected",
            ));
        }
        Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0)
    }

    /// Run the pump until `stop` is set (poll-driven; no busy spin).
    pub fn run(&mut self, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            if self.wait_readable(200)? {
                self.pump_once()?;
            }
        }
        Ok(())
    }

    /// Open a fresh read fd on the re-emit node — the fd handed to an app via `SCM_RIGHTS`.
    pub fn open_app_fd(&self) -> io::Result<OwnedFd> {
        let node = self
            .node_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "re-emit node not resolved"))?;
        open_read_fd(&node)
    }
}

impl ReportPump {
    fn new(remap: Remap) -> ReportPump {
        // The legacy uinput setup initializes every advertised ABS value to zero.
        let abs_state = remap
            .spec()
            .abs
            .iter()
            .map(|(code, _)| (*code, 0))
            .collect();
        ReportPump {
            remap,
            bucket: TokenBucket::default_broker(),
            pending_report: Vec::new(),
            pending_report_oversized: false,
            resynchronizing: false,
            pressed: HashSet::new(),
            abs_state,
            safe_return: None,
        }
    }

    /// `true` when the SafeReturn gate owns this canonical key transition (guide, flag set).
    fn consumed_by_safe_return(&mut self, code: u16, value: i32) -> bool {
        self.safe_return
            .as_mut()
            .is_some_and(|gate| gate.consume_key(code, value))
    }

    /// Route one batch of source events through remap + policy + the guide gate into `emit`.
    /// Returns events emitted.
    pub(crate) fn process(
        &mut self,
        events: &[libc::input_event],
        now: f64,
        source: &impl SourceState,
        emit: &mut impl FnMut(u16, u16, i32) -> io::Result<()>,
    ) -> io::Result<usize> {
        let mut emitted = 0usize;
        for ev in events {
            let t = ev.type_;
            if t == ioc::EV_SYN && ev.code == ioc::SYN_DROPPED {
                self.pending_report.clear();
                self.pending_report_oversized = false;
                self.resynchronizing = true;
            } else if self.resynchronizing {
                // Events following SYN_DROPPED belong to the unreliable tail of the overrun.
                // Once its boundary arrives, query authoritative state before accepting reports.
                if t == ioc::EV_SYN && ev.code == ioc::SYN_REPORT {
                    let out = self.resynchronize_source_state(source)?;
                    for (ty, code, value) in out {
                        emit(ty, code, value)?;
                        emitted += 1;
                    }
                    self.resynchronizing = false;
                }
            } else if t == ioc::EV_SYN && ev.code == ioc::SYN_REPORT {
                let allowed = self.bucket.allow(now);
                let out = finish_report(
                    &mut self.pending_report,
                    &mut self.pending_report_oversized,
                    &mut self.pressed,
                    allowed,
                );
                for (ty, code, value) in out {
                    emit(ty, code, value)?;
                    if ty == ioc::EV_ABS {
                        self.abs_state.insert(code, value);
                    }
                    emitted += 1;
                }
            } else if t == ioc::EV_KEY {
                let code = self.remap.remap_key(ev.code);
                if !self.consumed_by_safe_return(code, ev.value) {
                    push_report_event(
                        &mut self.pending_report,
                        &mut self.pending_report_oversized,
                        (t, code, ev.value),
                    );
                }
            } else if t == ioc::EV_ABS {
                // Analog axes pass through; a physically-binary trigger (semantics="binary") is
                // reclassified to an EV_KEY press/release on its canonical button (descriptor-driven).
                match self.remap.classify_abs(ev.code, ev.value) {
                    AbsAction::Passthrough => {
                        push_report_event(
                            &mut self.pending_report,
                            &mut self.pending_report_oversized,
                            (t, ev.code, ev.value),
                        );
                    }
                    AbsAction::Button { code, value } => {
                        if !self.consumed_by_safe_return(code, value) {
                            push_report_event(
                                &mut self.pending_report,
                                &mut self.pending_report_oversized,
                                (ioc::EV_KEY, code, value),
                            );
                        }
                    }
                    AbsAction::None => {} // inside the hysteresis band / no state change — drop
                }
            }
            // Other event types are outside the descriptor-controlled input surface.
        }
        Ok(emitted)
    }

    fn resynchronize_source_state(
        &mut self,
        source: &impl SourceState,
    ) -> io::Result<Vec<(u16, u16, i32)>> {
        let mut actual_pressed: HashSet<u16> = source
            .pressed_keys(self.remap.required_source_keys())?
            .into_iter()
            .map(|code| self.remap.remap_key(code))
            .collect();
        let abs_values = self
            .remap
            .required_source_abs()
            .iter()
            .copied()
            .map(|code| source.abs_value(code).map(|value| (code, value)))
            .collect::<io::Result<Vec<_>>>()?;
        let mut actual_abs = Vec::new();
        let mut actual_binary = Vec::new();
        for (code, value) in abs_values {
            match self.remap.resync_abs(code, value) {
                AbsAction::Passthrough => actual_abs.push((code, value)),
                AbsAction::Button { code, value } => actual_binary.push((code, value != 0)),
                AbsAction::None => unreachable!("resync_abs always yields authoritative state"),
            }
        }
        if let Some(gate) = self.safe_return.as_mut() {
            // The guide key never enters the visible state; its authoritative level feeds the
            // gate instead, so a press lost inside the overrun still returns to the launcher.
            let binary_guide = actual_binary
                .iter()
                .any(|&(code, down)| code == GUIDE_CODE && down);
            actual_binary.retain(|&(code, _)| code != GUIDE_CODE);
            let key_guide = actual_pressed.remove(&GUIDE_CODE);
            gate.set_down(key_guide || binary_guide);
        }
        Ok(diff_resynchronized_state(
            &mut self.pressed,
            &mut self.abs_state,
            actual_pressed,
            actual_abs,
            actual_binary,
        ))
    }
}

fn discover_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
    mut probe: impl FnMut(&Path) -> io::Result<bool>,
) -> io::Result<PathBuf> {
    let matches = candidates
        .into_iter()
        // A disappearing or non-evdev candidate is not a discovery-wide failure. The next
        // candidate may still be the descriptor-selected controller.
        .filter(|path| matches!(probe(path), Ok(true)))
        .collect();
    InputBroker::resolve_matches(matches)
}

fn finish_report(
    pending: &mut Vec<(u16, u16, i32)>,
    oversized: &mut bool,
    pressed: &mut HashSet<u16>,
    allowed: bool,
) -> Vec<(u16, u16, i32)> {
    let mut out = Vec::new();
    if allowed && !*oversized {
        for &(ty, code, value) in pending.iter() {
            if ty == ioc::EV_KEY {
                if value == 0 {
                    pressed.remove(&code);
                } else {
                    pressed.insert(code);
                }
            }
            out.push((ty, code, value));
        }
    } else {
        // A suppressed report may contain the release for any currently-visible press. Release
        // every visible key: this fail-safe direction cannot strand a button down.
        let mut releases: Vec<_> = pressed.drain().collect();
        releases.sort_unstable();
        out.extend(releases.into_iter().map(|code| (ioc::EV_KEY, code, 0)));
    }
    pending.clear();
    *oversized = false;
    // A rejected report produces no sink write at all unless releases are needed. In that case
    // the boundary commits those releases atomically. An admitted report always retains its
    // boundary, including an otherwise-empty report.
    if allowed || !out.is_empty() {
        out.push((ioc::EV_SYN, ioc::SYN_REPORT, 0));
    }
    out
}

fn diff_resynchronized_state(
    pressed: &mut HashSet<u16>,
    abs_state: &mut HashMap<u16, i32>,
    mut actual_pressed: HashSet<u16>,
    actual_abs: Vec<(u16, i32)>,
    actual_binary: Vec<(u16, bool)>,
) -> Vec<(u16, u16, i32)> {
    for (code, down) in actual_binary {
        if down {
            actual_pressed.insert(code);
        } else {
            actual_pressed.remove(&code);
        }
    }
    let mut changed_keys: Vec<_> = pressed
        .symmetric_difference(&actual_pressed)
        .copied()
        .collect();
    changed_keys.sort_unstable();
    let mut out: Vec<_> = changed_keys
        .into_iter()
        .map(|code| (ioc::EV_KEY, code, i32::from(actual_pressed.contains(&code))))
        .collect();
    *pressed = actual_pressed;
    for (code, value) in actual_abs {
        if abs_state.get(&code) != Some(&value) {
            out.push((ioc::EV_ABS, code, value));
            abs_state.insert(code, value);
        }
    }
    if !out.is_empty() {
        out.push((ioc::EV_SYN, ioc::SYN_REPORT, 0));
    }
    out
}

fn push_report_event(
    pending: &mut Vec<(u16, u16, i32)>,
    oversized: &mut bool,
    event: (u16, u16, i32),
) {
    if *oversized {
        return;
    }
    if pending.len() == MAX_REPORT_EVENTS {
        pending.clear();
        *oversized = true;
        return;
    }
    pending.push(event);
}

fn acquire_then_create<A, B>(
    acquire: impl FnOnce() -> io::Result<A>,
    create: impl FnOnce(&A) -> io::Result<B>,
) -> io::Result<(A, B)> {
    let acquired = acquire()?;
    let created = create(&acquired)?;
    Ok((acquired, created))
}

/// Open a node read-only, non-blocking, close-on-exec (the consumer's read fd shape).
pub fn open_read_fd(path: impl AsRef<Path>) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    // SAFETY: valid C string.
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// --- the Acquire("input") fd-handoff server (wire §4.1) -------------------------------------

/// Serve `Acquire("input")` on `listener`, handing the re-emit read fd over `SCM_RIGHTS`. Each
/// connection gets ONE acquisition then closes. Runs until `stop` is set.
pub fn serve_acquire(
    listener: &UnixListener,
    app_fd_path: &str,
    stop: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_acquire(stream, app_fd_path);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Handle one acquisition connection: reply to `Acquire("input")` with `Ok` + the fd; anything
/// else gets a typed error (this socket only vends input).
pub fn handle_acquire(mut stream: UnixStream, app_fd_path: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_nonblocking(false)?;
    let req = match recv_request(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()), // malformed / closed → drop
    };
    if req.op == Op::Acquire && req.name.eq_ignore_ascii_case("input") {
        let fd = open_read_fd(app_fd_path)?;
        let mut framed = Vec::new();
        send_response(&mut framed, &Response::ok()).map_err(wire_err)?; // framed PFW1 Response bytes
        scm::send_fd(stream.as_raw_fd(), &framed, fd.as_raw_fd())?;
    } else {
        // This socket vends only the input fd; everything else is unsupported here.
        let _ = send_response(&mut stream, &Response::err(Status::Unsupported));
    }
    Ok(())
}

/// Serve one app connection as a persistent PFW1 request/response loop (`tsp-f3fm.202.1`).
///
/// * `Acquire("input")` replies `Ok` plus a fresh re-emit read fd over `SCM_RIGHTS`, as many
///   times as the client asks, so a client that dropped its fd can re-acquire on the same session
///   connection.
/// * `GetAppearance` goes through [`pocketforge::server::handle_request`]: prefsd at
///   `$PF_PREFSD_SOCK` when set, otherwise `backend`.
/// * Every other op gets a typed `Unsupported`.
///
/// The client (the app's `BrokerClientBackend`) holds this connection for its whole session, so
/// the idle wait between requests is unbounded. Once a request's first byte arrives, the request
/// and its reply are each bounded by the acquisition I/O deadline, so a client that stalls
/// mid-frame cannot pin the thread. EOF, a hangup or a protocol error ends the loop.
pub fn serve_client(
    stream: UnixStream,
    app_fd_path: &str,
    backend: &dyn Backend,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    while wait_for_request(&stream)? {
        let req = match recv_request(&mut &stream) {
            Ok(r) => r,
            Err(_) => return Ok(()), // closed / malformed / stalled mid-frame → drop
        };
        respond(&stream, &req, app_fd_path, backend)?;
    }
    Ok(())
}

/// Block until the client sends something (`true`) or hangs up (`false`).
fn wait_for_request(stream: &UnixStream) -> io::Result<bool> {
    loop {
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; -1 waits until readable or hung up.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        // Pending bytes win over a simultaneous hangup: a request written just before close
        // is still answered (or fails cleanly at EOF).
        return Ok(pfd.revents & libc::POLLIN != 0);
    }
}

fn respond(
    stream: &UnixStream,
    req: &Request,
    app_fd_path: &str,
    backend: &dyn Backend,
) -> io::Result<()> {
    let mut writer = stream;
    match req.op {
        Op::Acquire if req.name.eq_ignore_ascii_case("input") => {
            let fd = match open_read_fd(app_fd_path) {
                Ok(fd) => fd,
                Err(e) => {
                    // Keep the session connection: a typed refusal, never a fabricated fd.
                    eprintln!("pf-input-broker: re-emit node {app_fd_path} unavailable: {e}");
                    return send_response(&mut writer, &Response::err(Status::HardwareAbsent))
                        .map_err(wire_err);
                }
            };
            let mut framed = Vec::new();
            send_response(&mut framed, &Response::ok()).map_err(wire_err)?;
            scm::send_fd(stream.as_raw_fd(), &framed, fd.as_raw_fd())
        }
        Op::GetAppearance => {
            send_response(&mut writer, &handle_request(backend, req)).map_err(wire_err)
        }
        // This socket vends the input fd and the appearance read; nothing else.
        _ => send_response(&mut writer, &Response::err(Status::Unsupported)).map_err(wire_err),
    }
}

/// Client side: `Acquire("input")` from the broker at `sock_path`, returning the PFW1 response +
/// the shared re-emit read fd. This is the `libpocketforge` input-acquisition path the `.2`
/// facade reserves — the fd, not RPC, is the hot path.
pub fn acquire_input_fd(sock_path: impl AsRef<Path>) -> io::Result<(Response, OwnedFd)> {
    use pf_wire::{recv_response, send_request};
    let mut stream = UnixStream::connect(sock_path)?;
    send_request(&mut stream, &Request::new(Op::Acquire, "input")).map_err(wire_err)?;

    let mut buf = [0u8; 256];
    let (n, fd) = scm::recv_fd(stream.as_raw_fd(), &mut buf)?;
    let mut cur = io::Cursor::new(&buf[..n]);
    let resp = recv_response(&mut cur).map_err(wire_err)?;
    match fd {
        Some(fd) if resp.status == Status::Ok => Ok((resp, fd)),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker refused input",
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker sent no fd",
        )),
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn systemd_decoder_restart_is_recovered_by_broker_failure_restart() {
        let unit = include_str!("../systemd/pf-input-broker.service");
        assert!(unit.lines().any(|line| {
            line == "After=local-fs.target dev-uinput.device pf-input-decode.service"
        }));
        assert!(unit.lines().any(|line| line == "Restart=on-failure"));
        assert!(
            !unit.lines().any(|line| {
                line.starts_with("BindsTo=")
                    || (line.starts_with("Requires=")
                        && line.contains("pf-input-decode.service"))
            }),
            "decoder lifetime must not stop the broker cleanly: fd loss must fail the pump, restart the broker, rediscover the decoder, and take a fresh grab"
        );
    }

    #[test]
    fn identity_match_rejects_wrong_name_or_id() {
        let e = ExpectedIdentity {
            name: "TRIMUI Player1".into(),
            bus: 3,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
        };
        assert!(e.matches("TRIMUI Player1", (3, 0x045e, 0x028e, 0x0110)));
        assert!(!e.matches("event-gamepad", (3, 0x045e, 0x028e, 0x0110)));
        assert!(!e.matches("TRIMUI Player1", (3, 0x1234, 0x028e, 0x0110)));
        assert!(!e.matches("TRIMUI Player1", (5, 0x045e, 0x028e, 0x0110)));
    }

    #[test]
    fn discovery_skips_failing_candidate_before_valid_match() {
        let bad = PathBuf::from("/dev/input/event0");
        let good = PathBuf::from("/dev/input/event1");
        let found = discover_candidates(vec![bad.clone(), good.clone()], |path| {
            if path == bad {
                Err(io::Error::new(io::ErrorKind::NotFound, "unplugged"))
            } else {
                Ok(true)
            }
        })
        .unwrap();
        assert_eq!(found, good);
    }

    #[test]
    fn syn_dropped_resyncs_actual_keys_and_axes_without_stale_state() {
        let mut pressed = HashSet::from([0x130, 0x131]);
        let mut abs = HashMap::from([(0, 10), (1, 20)]);
        assert_eq!(
            diff_resynchronized_state(
                &mut pressed,
                &mut abs,
                HashSet::from([0x130, 0x132]),
                vec![(0, 10), (1, 99)],
                vec![],
            ),
            vec![
                (ioc::EV_KEY, 0x131, 0),
                (ioc::EV_KEY, 0x132, 1),
                (ioc::EV_ABS, 1, 99),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
        assert_eq!(pressed, HashSet::from([0x130, 0x132]));
        assert_eq!(abs, HashMap::from([(0, 10), (1, 99)]));
    }

    #[test]
    fn syn_dropped_keeps_key_pressed_across_overrun_and_releases_actual_up_key() {
        let mut pressed = HashSet::from([0x130, 0x131]);
        let mut abs = HashMap::new();
        assert_eq!(
            diff_resynchronized_state(
                &mut pressed,
                &mut abs,
                HashSet::from([0x130]),
                vec![],
                vec![],
            ),
            vec![(ioc::EV_KEY, 0x131, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(
            pressed.contains(&0x130),
            "held-across-drop key stays pressed"
        );
        assert!(
            !pressed.contains(&0x131),
            "key released in dropped tail converges up"
        );
    }

    #[test]
    fn suppressed_report_synthesizes_release_for_visible_press() {
        let mut pressed = HashSet::from([0x130]);
        let mut report = vec![(ioc::EV_KEY, 0x130, 0)];
        assert_eq!(
            finish_report(&mut report, &mut false, &mut pressed, false),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(pressed.is_empty());
    }

    #[test]
    fn allowed_report_keeps_all_payload_with_its_syn_report() {
        let mut pressed = HashSet::new();
        let mut report = vec![(ioc::EV_KEY, 0x130, 1), (ioc::EV_ABS, 0, 17)];
        assert_eq!(
            finish_report(&mut report, &mut false, &mut pressed, true),
            vec![
                (ioc::EV_KEY, 0x130, 1),
                (ioc::EV_ABS, 0, 17),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0)
            ]
        );
        assert!(pressed.contains(&0x130));
    }

    #[test]
    fn rejected_reports_emit_nothing_after_required_release_commit() {
        let mut pressed = HashSet::from([0x130]);
        let mut oversized = false;
        let mut report = vec![(ioc::EV_KEY, 0x130, 1)];
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, false),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        for _ in 0..1_000 {
            report.push((ioc::EV_ABS, 0, 1));
            assert!(finish_report(&mut report, &mut oversized, &mut pressed, false).is_empty());
        }
    }

    #[test]
    fn oversized_report_is_bounded_released_and_resynchronizes() {
        let mut pressed = HashSet::from([0x130]);
        let mut oversized = false;
        let mut report = Vec::new();
        for _ in 0..MAX_REPORT_EVENTS + 10_000 {
            push_report_event(&mut report, &mut oversized, (ioc::EV_ABS, 0, 1));
        }
        assert!(oversized);
        assert!(report.len() <= MAX_REPORT_EVENTS);
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, true),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        push_report_event(&mut report, &mut oversized, (ioc::EV_KEY, 0x131, 1));
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, true),
            vec![(ioc::EV_KEY, 0x131, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
    }

    #[test]
    fn stalled_acquire_client_is_deadline_bounded() {
        let (server, _silent_client) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        handle_acquire(server, "/unused").unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn grab_failure_never_creates_sink() {
        use std::cell::Cell;
        let created = Cell::new(false);
        let result: io::Result<((), ())> = acquire_then_create(
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "grab failed",
                ))
            },
            |_| {
                created.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(
            !created.get(),
            "sink factory must not run before successful acquisition"
        );
    }
}

/// Hermetic tests for the tsp-f3fm.202.1 additions: the persistent PFW1 session loop, the
/// GetAppearance pass-through, and the protected SafeReturn gate inside the report pump.
#[cfg(test)]
mod session_and_safe_return_tests {
    use super::*;
    use crate::safe_return::{SafeReturnIntake, CONNECT_TIMEOUT, IO_TIMEOUT, SAFE_RETURN_BODY};
    use pocketforge::backends::{BrokerClientBackend, InProcessBackend};
    use std::io::Write;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SOUTH: u16 = 0x130;

    fn descriptor() -> Descriptor {
        Descriptor::from_toml(
            r#"
[identity]
id = "synthguide"
manufacturer = "PocketForge"
model = "Guide Rig (synthetic test descriptor)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[inputs]]
id = "guide"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_MODE"

[[inputs]]
id = "ltrig"
kind = "trigger"
ev_type = "EV_ABS"
code = "ABS_Z"
range = { min = 0, max = 255, fuzz = 0, flat = 0 }
"#,
        )
        .unwrap()
    }

    fn pump(intake: Option<SafeReturnIntake>) -> ReportPump {
        let mut pump = ReportPump::new(Remap::from_descriptor(&descriptor()).unwrap());
        pump.safe_return = intake.map(SafeReturnGate::new);
        pump
    }

    #[derive(Default)]
    struct FakeSource {
        pressed: Vec<u16>,
    }
    impl SourceState for FakeSource {
        fn pressed_keys(&self, codes: &[u16]) -> io::Result<Vec<u16>> {
            Ok(codes
                .iter()
                .copied()
                .filter(|c| self.pressed.contains(c))
                .collect())
        }
        fn abs_value(&self, _code: u16) -> io::Result<i32> {
            Ok(0)
        }
    }

    fn ev(ty: u16, code: u16, value: i32) -> libc::input_event {
        // SAFETY: input_event is plain old data.
        let mut e: libc::input_event = unsafe { std::mem::zeroed() };
        e.type_ = ty;
        e.code = code;
        e.value = value;
        e
    }
    fn key(code: u16, value: i32) -> libc::input_event {
        ev(ioc::EV_KEY, code, value)
    }
    fn syn() -> libc::input_event {
        ev(ioc::EV_SYN, ioc::SYN_REPORT, 0)
    }

    fn run(pump: &mut ReportPump, events: &[libc::input_event]) -> Vec<(u16, u16, i32)> {
        run_with(pump, events, &FakeSource::default())
    }
    fn run_with(
        pump: &mut ReportPump,
        events: &[libc::input_event],
        source: &FakeSource,
    ) -> Vec<(u16, u16, i32)> {
        let mut out = Vec::new();
        pump.process(events, 0.0, source, &mut |t, c, v| {
            out.push((t, c, v));
            Ok(())
        })
        .unwrap();
        out
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pf-input-broker-{tag}-{}", std::process::id()))
    }

    fn intake(sock: &Path) -> (SafeReturnIntake, Receiver<String>) {
        let (tx, rx) = channel();
        let intake = SafeReturnIntake::spawn_with(
            sock,
            CONNECT_TIMEOUT,
            IO_TIMEOUT,
            Box::new(move |line| {
                let _ = tx.send(line.to_owned());
            }),
        )
        .unwrap();
        (intake, rx)
    }

    fn guide_press_release() -> Vec<libc::input_event> {
        vec![key(GUIDE_CODE, 1), syn(), key(GUIDE_CODE, 0), syn()]
    }

    #[test]
    fn without_flag_guide_passes_through_unchanged() {
        let mut p = pump(None);
        assert_eq!(
            run(&mut p, &guide_press_release()),
            vec![
                (ioc::EV_KEY, GUIDE_CODE, 1),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_KEY, GUIDE_CODE, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
    }

    #[test]
    fn guide_is_suppressed_while_other_codes_pass_through() {
        let sock = scratch("suppress-absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, _log) = intake(&sock);
        let mut p = pump(Some(intake));
        let out = run(
            &mut p,
            &[
                key(SOUTH, 1),
                key(GUIDE_CODE, 1),
                syn(),
                key(GUIDE_CODE, 2),
                syn(),
                key(GUIDE_CODE, 0),
                key(SOUTH, 0),
                syn(),
            ],
        );
        assert_eq!(
            out,
            vec![
                (ioc::EV_KEY, SOUTH, 1),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_KEY, SOUTH, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));
        assert!(!p.pressed.contains(&GUIDE_CODE));
    }

    #[test]
    fn fake_authority_receives_exactly_one_frame_per_press() {
        let sock = scratch("one-frame.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (frames_tx, frames) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let body = pf_wire::read_frame(&mut stream).unwrap();
                        pf_wire::write_frame(&mut stream, br#"{"result":"ok"}"#).unwrap();
                        frames_tx.send(body).unwrap();
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        for press in 1..=2 {
            // A press, autorepeat and release are one press edge.
            run(
                &mut p,
                &[
                    key(GUIDE_CODE, 1),
                    syn(),
                    key(GUIDE_CODE, 2),
                    syn(),
                    key(GUIDE_CODE, 0),
                    syn(),
                ],
            );
            let line = log.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(line.ends_with(": ok"), "press {press}: {line}");
            assert_eq!(
                frames.recv_timeout(Duration::from_secs(1)).unwrap(),
                SAFE_RETURN_BODY
            );
        }
        assert!(
            frames.recv_timeout(Duration::from_millis(300)).is_err(),
            "no frame beyond one per press"
        );
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn stalled_authority_never_blocks_the_input_pump() {
        // Bound but never accepting or answering: a single-threaded authority stuck in
        // `systemctl stop`. The worker waits out its full production 5 s read timeout.
        let sock = scratch("stalled.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (intake, _log) = intake(&sock);
        let mut p = pump(Some(intake));
        let mut events = Vec::new();
        for _ in 0..50 {
            events.extend(guide_press_release());
            events.extend([key(SOUTH, 1), syn(), key(SOUTH, 0), syn()]);
        }
        let started = Instant::now();
        let out = run(&mut p, &events);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1), "pump took {elapsed:?}");
        let south: Vec<_> = out.iter().filter(|e| e.1 == SOUTH).collect();
        assert_eq!(south.len(), 100, "every non-guide event still flows");
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));
        assert!(
            p.safe_return.as_ref().unwrap().intake().in_flight(),
            "the first press is still waiting on the authority"
        );
        // 50 presses while that request is in flight coalesce into exactly one connection.
        let deadline = Instant::now() + Duration::from_secs(1);
        let first = loop {
            match listener.accept() {
                Ok(conn) => break conn,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("no SafeReturn connection: {e}"),
            }
        };
        drop(first);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "coalesced presses must not open more connections"
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn absent_authority_is_logged_and_the_pump_continues() {
        let sock = scratch("absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        let out = run(&mut p, &guide_press_release());
        let line = log.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            line.contains("safe_return") && line.contains("failed") && line.contains("NotFound"),
            "{line}"
        );
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));
        let deadline = Instant::now() + Duration::from_secs(1);
        while p.safe_return.as_ref().unwrap().intake().in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let out = run(&mut p, &[key(SOUTH, 1), syn()]);
        assert_eq!(
            out,
            vec![(ioc::EV_KEY, SOUTH, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        run(&mut p, &guide_press_release());
        let again = log.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(again.contains("failed"), "the next press retries: {again}");
    }

    #[test]
    fn syn_dropped_resync_withholds_guide_and_counts_its_press() {
        let sock = scratch("resync-absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        let source = FakeSource {
            pressed: vec![SOUTH, GUIDE_CODE],
        };
        let out = run_with(
            &mut p,
            &[
                ev(ioc::EV_SYN, ioc::SYN_DROPPED, 0),
                key(GUIDE_CODE, 1),
                syn(),
            ],
            &source,
        );
        assert_eq!(
            out,
            vec![(ioc::EV_KEY, SOUTH, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(!p.pressed.contains(&GUIDE_CODE));
        assert!(
            log.recv_timeout(Duration::from_secs(2)).is_ok(),
            "press edge requested"
        );
    }

    // --- the persistent PFW1 session loop -----------------------------------------------------

    fn node_file(tag: &str) -> PathBuf {
        let path = scratch(tag);
        std::fs::write(&path, b"re-emit-node").unwrap();
        path
    }

    fn backend() -> Arc<dyn Backend> {
        Arc::new(InProcessBackend::new(Arc::new(descriptor())))
    }

    fn serve_pair(node: &Path) -> (UnixStream, std::thread::JoinHandle<io::Result<()>>) {
        let (client, server) = UnixStream::pair().unwrap();
        let node = node.to_str().unwrap().to_owned();
        let backend = backend();
        let handle = std::thread::spawn(move || serve_client(server, &node, &*backend));
        (client, handle)
    }

    fn acquire_on(client: &UnixStream) -> (Response, Option<OwnedFd>) {
        pf_wire::send_request(&mut &*client, &Request::new(Op::Acquire, "input")).unwrap();
        let mut buf = [0u8; 256];
        let (n, fd) = scm::recv_fd(client.as_raw_fd(), &mut buf).unwrap();
        let resp = pf_wire::recv_response(&mut io::Cursor::new(&buf[..n])).unwrap();
        (resp, fd)
    }

    fn call(client: &UnixStream, op: Op, name: &str) -> Response {
        pf_wire::send_request(&mut &*client, &Request::new(op, name)).unwrap();
        pf_wire::recv_response(&mut &*client).unwrap()
    }

    #[test]
    fn two_acquires_on_one_connection_both_return_fds() {
        let node = node_file("two-acquires.node");
        let (client, server) = serve_pair(&node);
        let (first, fd1) = acquire_on(&client);
        let (second, fd2) = acquire_on(&client);
        assert_eq!(first.status, Status::Ok);
        assert_eq!(second.status, Status::Ok);
        let (fd1, fd2) = (fd1.expect("first fd"), fd2.expect("second fd"));
        assert_ne!(fd1.as_raw_fd(), fd2.as_raw_fd());
        for fd in [&fd1, &fd2] {
            let mut buf = [0u8; 12];
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            assert_eq!(&buf[..n as usize], b"re-emit-node");
        }
        drop(client);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn get_appearance_is_served_and_other_ops_stay_unsupported() {
        let node = node_file("appearance.node");
        let (client, server) = serve_pair(&node);
        let appearance = call(&client, Op::GetAppearance, "");
        assert_eq!(appearance.status, Status::Ok);
        assert!(
            appearance.flag <= 2,
            "a valid Appearance: {}",
            appearance.flag
        );
        for (op, name) in [
            (Op::IsPresent, "input"),
            (Op::GetAppearanceSource, ""),
            (Op::GetPreference, ""),
            (Op::Acquire, "imu"),
        ] {
            assert_eq!(
                call(&client, op, name).status,
                Status::Unsupported,
                "{op:?}"
            );
        }
        // The session survives all of that: input can still be acquired.
        assert!(acquire_on(&client).1.is_some());
        drop(client);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn broker_client_backend_reacquires_after_dropping_its_fd() {
        let node = node_file("reacquire.node");
        let (client, server) = serve_pair(&node);
        let be = BrokerClientBackend::from_stream(client);
        let fd = be.acquire_input_fd().expect("first acquire");
        drop(fd);
        let _ = be.appearance(); // a facade poll between acquisitions keeps the session
        let fd = be.acquire_input_fd().expect("re-acquire after drop");
        let mut buf = [0u8; 12];
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(&buf[..n as usize], b"re-emit-node");
        drop(be);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn missing_node_is_a_typed_refusal_and_the_session_survives() {
        let node = scratch("missing.node");
        let _ = std::fs::remove_file(&node);
        let (client, server) = serve_pair(&node);
        pf_wire::send_request(&mut &client, &Request::new(Op::Acquire, "input")).unwrap();
        let resp = pf_wire::recv_response(&mut &client).unwrap();
        assert_eq!(resp.status, Status::HardwareAbsent);
        assert_eq!(call(&client, Op::GetAppearance, "").status, Status::Ok);
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn client_stalled_mid_frame_is_deadline_bounded() {
        let node = node_file("stalled-frame.node");
        let (mut client, server) = serve_pair(&node);
        client.write_all(&[0, 0]).unwrap(); // half a length prefix, then silence
        let started = Instant::now();
        server.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_file(node);
    }
}
