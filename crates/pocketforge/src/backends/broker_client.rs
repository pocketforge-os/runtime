//! The **out-of-process broker client backend** — speaks PFW1 over a Unix socket to the
//! broker (the reference [`crate::server`] today; the real default-deny daemon in `.3`).
//!
//! This is the OTHER implementation of [`Backend`] behind the IDENTICAL facade. Constructing a
//! [`crate::Pf`] over this instead of [`super::InProcessBackend`] is the only change needed to
//! run an app against the out-of-process broker — no app-source change. That is the
//! backend-swap proof.
//!
//! **Fail-closed transport policy:** the [`Backend`] trait methods are infallible (they return
//! values, not `io::Result`), but a socket can drop. On any transport error this backend logs
//! to stderr and returns the *conservative* answer (absent / denied / no-op) — never a
//! fabricated grant. A healthy socket (the normal case, and every test) returns the broker's
//! real answer.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use pf_wire::{recv_response, send_request, Op, Request, Response, RumbleStatus};

use crate::backend::{Backend, Pose};
use crate::backends::scm;
use crate::error::{CapError, PermissionState};

/// A client that talks PFW1 to a broker over one Unix-socket connection.
pub struct BrokerClientBackend {
    stream: Mutex<UnixStream>,
}

impl BrokerClientBackend {
    /// Connect to a broker listening at `path`.
    pub fn connect(path: impl AsRef<Path>) -> std::io::Result<BrokerClientBackend> {
        Ok(BrokerClientBackend::from_stream(UnixStream::connect(path)?))
    }

    /// Wrap an already-connected stream (used by tests + when the supervisor hands the fd in).
    pub fn from_stream(stream: UnixStream) -> BrokerClientBackend {
        BrokerClientBackend { stream: Mutex::new(stream) }
    }

    /// One request/response round-trip. `None` on transport failure (fail-closed at call sites).
    fn call(&self, req: &Request) -> Option<Response> {
        let mut s = self.stream.lock().unwrap();
        if let Err(e) = send_request(&mut *s, req) {
            let _ = writeln!(std::io::stderr(), "pocketforge: broker send failed: {e}");
            return None;
        }
        match recv_response(&mut *s) {
            Ok(r) => Some(r),
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "pocketforge: broker recv failed: {e}");
                None
            }
        }
    }
}

impl Backend for BrokerClientBackend {
    fn is_present(&self, name: &str) -> bool {
        self.call(&Request::new(Op::IsPresent, name)).map(|r| r.flag != 0).unwrap_or(false)
    }

    fn is_granted(&self, name: &str) -> bool {
        self.call(&Request::new(Op::IsGranted, name)).map(|r| r.flag != 0).unwrap_or(false)
    }

    fn query(&self, name: &str) -> PermissionState {
        match self.call(&Request::new(Op::Query, name)) {
            Some(r) => PermissionState::from_wire(r.permission),
            None => PermissionState::Denied, // fail-closed
        }
    }

    fn acquire(&self, name: &str) -> Result<(), CapError> {
        match self.call(&Request::new(Op::Acquire, name)) {
            Some(r) => match CapError::from_status(r.status) {
                None => Ok(()),
                Some(e) => Err(e),
            },
            None => Err(CapError::PolicyBlocked), // fail-closed: transport down ⇒ not granted
        }
    }

    fn acquire_input_fd(&self) -> Result<OwnedFd, CapError> {
        // The input hot path is a HANDED FD, never per-event RPC (wire §5). PFW1 carries only the
        // *acquisition*: send `Acquire("input")` (no new Op — wire §4.1) and the broker replies
        // with the framed Response AS THE PAYLOAD plus the EVIOCGRAB-grabbed re-emit read fd as
        // one SCM_RIGHTS control message, read together in a single `recvmsg`.
        //
        // Fail-closed transport policy (as elsewhere in this backend): any send/recv/decode
        // failure returns the conservative taxonomy, never a fabricated fd.
        let s = self.stream.lock().unwrap();
        if let Err(e) = send_request(&mut &*s, &Request::new(Op::Acquire, "input")) {
            let _ = writeln!(std::io::stderr(), "pocketforge: broker input-fd send failed: {e}");
            return Err(CapError::PolicyBlocked);
        }
        let mut buf = [0u8; 256];
        let (n, fd) = match scm::recv_fd(s.as_raw_fd(), &mut buf) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "pocketforge: broker input-fd recvmsg failed: {e}");
                return Err(CapError::PolicyBlocked);
            }
        };
        let resp = match recv_response(&mut std::io::Cursor::new(&buf[..n])) {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "pocketforge: broker input-fd decode failed: {e}");
                return Err(CapError::PolicyBlocked);
            }
        };
        match (CapError::from_status(resp.status), fd) {
            (None, Some(fd)) => Ok(fd),                    // Ok + fd → the shared re-emit read fd
            (None, None) => Err(CapError::HardwareAbsent), // Ok but no fd (broker has no node)
            (Some(e), _) => Err(e),                        // typed broker refusal (drops any fd)
        }
    }

    fn rumble_pulse(&self, ms: u32) -> RumbleStatus {
        let mut req = Request::new(Op::RumblePulse, "rumble");
        req.arg = ms as u64;
        match self.call(&req) {
            Some(r) => RumbleStatus::from_u64(r.flag).unwrap_or(RumbleStatus::NoopAbsent),
            None => RumbleStatus::NoopAbsent, // fail-closed: safe cosmetic no-op
        }
    }

    fn get_pose(&self) -> Result<Pose, CapError> {
        match self.call(&Request::new(Op::GetPose, "imu")) {
            Some(r) => match CapError::from_status(r.status) {
                None => Pose::from_bytes(&r.payload).ok_or(CapError::Unsupported),
                Some(e) => Err(e),
            },
            None => Err(CapError::HardwareAbsent),
        }
    }

    fn set_pose(&self, pose: Pose) -> Result<Pose, CapError> {
        let mut req = Request::new(Op::SetPose, "imu");
        req.payload = pose.to_bytes().to_vec();
        match self.call(&req) {
            Some(r) => match CapError::from_status(r.status) {
                None => Pose::from_bytes(&r.payload).ok_or(CapError::Unsupported),
                Some(e) => Err(e),
            },
            None => Err(CapError::HardwareAbsent),
        }
    }

    fn get_capability(&self, name: &str) -> Result<Vec<u8>, CapError> {
        match self.call(&Request::new(Op::GetCapability, name)) {
            Some(r) => match CapError::from_status(r.status) {
                None => Ok(r.payload),
                Some(e) => Err(e),
            },
            None => Err(CapError::PolicyBlocked),
        }
    }

    fn set_capability(&self, name: &str, value: &[u8]) -> Result<(), CapError> {
        let mut req = Request::new(Op::SetCapability, name);
        req.payload = value.to_vec();
        match self.call(&req) {
            Some(r) => match CapError::from_status(r.status) {
                None => Ok(()),
                Some(e) => Err(e),
            },
            None => Err(CapError::PolicyBlocked),
        }
    }

    fn preference_bool(&self, _name: &str, default: bool) -> bool {
        // Preferences are not yet a wire op (E4 adds them); v0 returns the caller's default.
        default
    }

    fn set_preference_bool(&self, _name: &str, _value: bool) {
        // No-op over the wire in v0 (E4 adds a preference op + server-side store).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_wire::{recv_request, send_response};

    /// Device-free proof of the broker-side `acquire_input_fd` path: a fake broker on a socketpair
    /// receives `Acquire("input")` and replies with a framed `Response::ok()` payload + a real fd
    /// over `SCM_RIGHTS`; the backend must hand that fd back, readable, with no new wire `Op`.
    #[test]
    fn receives_input_fd_via_scm_rights() {
        let (client, server) = UnixStream::pair().unwrap();

        let srv = std::thread::spawn(move || {
            let mut s = server;
            let req = recv_request(&mut s).expect("recv Acquire");
            assert_eq!(req.op, Op::Acquire);
            assert!(req.name.eq_ignore_ascii_case("input"), "must ask for input");

            // A readable fd to hand over: a pipe holding known bytes (stands in for the re-emit
            // read fd the real broker opens on its EVIOCGRAB'd uinput device).
            let mut fds = [0i32; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let (rd, wr) = (fds[0], fds[1]);
            let payload = b"HELLO-FD";
            unsafe {
                libc::write(wr, payload.as_ptr() as *const libc::c_void, payload.len());
                libc::close(wr);
            }
            let mut framed = Vec::new();
            send_response(&mut framed, &Response::ok()).expect("frame response");
            scm::send_fd(s.as_raw_fd(), &framed, rd).expect("send fd");
            unsafe { libc::close(rd) }; // the server drops its copy; the client owns its own
        });

        let be = BrokerClientBackend::from_stream(client);
        let fd = be.acquire_input_fd().expect("client receives the fd");

        let mut buf = [0u8; 8];
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert_eq!(n, 8, "read the handed fd");
        assert_eq!(&buf, b"HELLO-FD", "the fd is the exact one the broker handed over");

        srv.join().unwrap();
    }

    /// A typed broker refusal (`Status::HardwareAbsent`) with no fd surfaces as the taxonomy, not
    /// a fabricated descriptor — the fail-closed contract.
    #[test]
    fn typed_refusal_when_broker_denies() {
        let (client, server) = UnixStream::pair().unwrap();
        let srv = std::thread::spawn(move || {
            let mut s = server;
            let _ = recv_request(&mut s).expect("recv Acquire");
            // Reply with a framed error and NO fd (there is no data-byte carrier for SCM_RIGHTS,
            // so send the framed response as an ordinary write).
            let _ = s.write_all(&{
                let mut f = Vec::new();
                send_response(&mut f, &Response::err(pf_wire::Status::HardwareAbsent)).unwrap();
                f
            });
        });
        let be = BrokerClientBackend::from_stream(client);
        assert_eq!(be.acquire_input_fd().err(), Some(CapError::HardwareAbsent));
        srv.join().unwrap();
    }
}
