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

use crate::evdev::Evdev;
use crate::ioc;
use crate::policy::TokenBucket;
use crate::remap::{AbsAction, Remap};
use crate::scm;
use crate::uinput::Uinput;

const MAX_REPORT_EVENTS: usize = 256;
const ACQUIRE_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

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
    remap: Remap,
    bucket: TokenBucket,
    start: std::time::Instant,
    pending_report: Vec<(u16, u16, i32)>,
    pending_report_oversized: bool,
    resynchronizing: bool,
    pressed: HashSet<u16>,
    abs_state: HashMap<u16, i32>,
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
        // The legacy uinput setup initializes every advertised ABS value to zero.
        let abs_state = remap
            .spec()
            .abs
            .iter()
            .map(|(code, _)| (*code, 0))
            .collect();
        Ok(InputBroker {
            source,
            sink,
            remap,
            bucket: TokenBucket::default_broker(),
            start: std::time::Instant::now(),
            pending_report: Vec::new(),
            pending_report_oversized: false,
            resynchronizing: false,
            pressed: HashSet::new(),
            abs_state,
        })
    }

    fn validate_source(source: &Evdev, descriptor: &Descriptor, remap: &Remap) -> io::Result<()> {
        let expected = ExpectedIdentity::from_descriptor(descriptor)?;
        if !expected.matches(&source.name()?, source.id()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened source identity mismatch",
            ));
        }
        if !source.supports(ioc::EV_KEY, remap.required_source_keys())?
            || !source.supports(ioc::EV_ABS, remap.required_source_abs())?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened source lacks descriptor-required capabilities",
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
                && dev.supports(ioc::EV_KEY, remap.required_source_keys())?
                && dev.supports(ioc::EV_ABS, remap.required_source_abs())?)
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
        let mut emitted = 0usize;
        for ev in &buf[..n] {
            let t = ev.type_;
            if t == ioc::EV_SYN && ev.code == ioc::SYN_DROPPED {
                self.pending_report.clear();
                self.pending_report_oversized = false;
                self.resynchronizing = true;
            } else if self.resynchronizing {
                // Events following SYN_DROPPED belong to the unreliable tail of the overrun.
                // Once its boundary arrives, query authoritative state before accepting reports.
                if t == ioc::EV_SYN && ev.code == ioc::SYN_REPORT {
                    let out = self.resynchronize_source_state()?;
                    for (ty, code, value) in out {
                        self.sink.emit(ty, code, value)?;
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
                    self.sink.emit(ty, code, value)?;
                    if ty == ioc::EV_ABS {
                        self.abs_state.insert(code, value);
                    }
                    emitted += 1;
                }
            } else if t == ioc::EV_KEY {
                push_report_event(
                    &mut self.pending_report,
                    &mut self.pending_report_oversized,
                    (t, self.remap.remap_key(ev.code), ev.value),
                );
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
                        push_report_event(
                            &mut self.pending_report,
                            &mut self.pending_report_oversized,
                            (ioc::EV_KEY, code, value),
                        );
                    }
                    AbsAction::None => {} // inside the hysteresis band / no state change — drop
                }
            }
            // Other event types are outside the descriptor-controlled input surface.
        }
        Ok(emitted)
    }

    fn resynchronize_source_state(&mut self) -> io::Result<Vec<(u16, u16, i32)>> {
        let actual_pressed: HashSet<u16> = self
            .source
            .pressed_keys(self.remap.required_source_keys())?
            .into_iter()
            .map(|code| self.remap.remap_key(code))
            .collect();
        let abs_values = self
            .remap
            .required_source_abs()
            .iter()
            .copied()
            .map(|code| self.source.abs_value(code).map(|value| (code, value)))
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
        Ok(diff_resynchronized_state(
            &mut self.pressed,
            &mut self.abs_state,
            actual_pressed,
            actual_abs,
            actual_binary,
        ))
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
