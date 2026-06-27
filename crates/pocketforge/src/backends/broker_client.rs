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
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use pf_wire::{recv_response, send_request, Op, Request, Response, RumbleStatus};

use crate::backend::{Backend, Pose};
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
