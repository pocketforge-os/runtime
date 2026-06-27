//! The **input broker** — the load-bearing v0 enforcement: open the real evdev source,
//! `EVIOCGRAB` it (exclusive), and pump its events through the descriptor remap + the rate-limit
//! policy into a uinput re-emit device the app reads. The app gets the re-emit read fd via
//! `Acquire("input")` over a Unix socket (`SCM_RIGHTS`); it can no longer reach the real node.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use pf_wire::{recv_request, send_response, Op, Request, Response, Status};
use pocketforge::descriptor::Descriptor;

use crate::evdev::Evdev;
use crate::ioc;
use crate::policy::TokenBucket;
use crate::remap::Remap;
use crate::scm;
use crate::uinput::Uinput;

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
}

impl InputBroker {
    /// Open `source_path`, grab it (the enforcing default), and stand up the descriptor-derived
    /// re-emit device.
    pub fn start(source_path: impl AsRef<Path>, descriptor: &Descriptor) -> io::Result<InputBroker> {
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
        let sink = Uinput::create(remap.spec())?;
        let mut source = Evdev::open(source_path)?;
        if grab {
            source.grab()?; // EXCLUSIVE — the app cannot bypass to the raw node (enforcement).
        }
        Ok(InputBroker { source, sink, remap, bucket: TokenBucket::default_broker(), start: std::time::Instant::now() })
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
            if t == ioc::EV_SYN {
                self.sink.emit(t, ev.code, ev.value)?;
                emitted += 1;
            } else if t == ioc::EV_KEY && self.bucket.allow(now) {
                self.sink.emit(t, self.remap.remap_key(ev.code), ev.value)?;
                emitted += 1;
            } else if t == ioc::EV_ABS && self.bucket.allow(now) {
                self.sink.emit(t, ev.code, ev.value)?; // axes are already canonical
                emitted += 1;
            }
            // EV_KEY/EV_ABS over the rate-limit budget, and other types (FF, MSC, …), are dropped.
        }
        Ok(emitted)
    }

    /// Block up to `timeout_ms` for the source to become readable. `true` if events are pending.
    pub fn wait_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.source.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: single valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(e);
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

/// Open a node read-only, non-blocking, close-on-exec (the consumer's read fd shape).
pub fn open_read_fd(path: impl AsRef<Path>) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    // SAFETY: valid C string.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// --- the Acquire("input") fd-handoff server (wire §4.1) -------------------------------------

/// Serve `Acquire("input")` on `listener`, handing the re-emit read fd over `SCM_RIGHTS`. Each
/// connection gets ONE acquisition then closes. Runs until `stop` is set.
pub fn serve_acquire(listener: &UnixListener, app_fd_path: &str, stop: &AtomicBool) -> io::Result<()> {
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
        Some(_) => Err(io::Error::new(io::ErrorKind::PermissionDenied, "broker refused input")),
        None => Err(io::Error::new(io::ErrorKind::InvalidData, "broker sent no fd")),
    }
}
