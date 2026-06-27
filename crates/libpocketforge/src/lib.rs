//! # `libpocketforge` — the C ABI for the capability facade
//!
//! A thin `extern "C"` wrapper over the `pocketforge` crate so any-language OCI app can link
//! `libpocketforge.{so,a}` and call the broker. The numeric enums here are FROZEN as part of
//! the public contract (`tsp-e1b.5`) and match `pf_wire`'s wire enums + the hand-written
//! header `include/pocketforge.h` (kept in sync; the C smoke test in `ctest/` proves it links
//! and runs).
//!
//! Memory rules: `pf_connect*` returns an owning `*mut PfSession` (NULL on error) that the
//! caller frees exactly once with `pf_free`. All `name` arguments are borrowed NUL-terminated
//! UTF-8; the library copies what it needs. No pointer the library returns must be freed by C
//! except the session.

use std::ffi::{c_char, CStr};
use std::ptr;

use pf::{Descriptor, Pf};

/// Opaque session handle (a boxed [`pf::Pf`]).
pub struct PfSession {
    pf: Pf,
}

/// Two-stage presence, returned by value (matches `PfPresence` in the header).
#[repr(C)]
pub struct PfPresence {
    /// 1 if the capability type exists in this runtime build.
    pub api: i32,
    /// 1 if the descriptor + probe back it with real hardware on this device.
    pub hardware: i32,
}

// Status / permission / rumble integer codes — FROZEN, == pf_wire's wire values.
/// `pf_acquire` success.
pub const PF_OK: i32 = 0;
/// No such capability on this platform.
pub const PF_UNSUPPORTED: i32 = 1;
/// Refused by manifest/tier policy.
pub const PF_POLICY_BLOCKED: i32 = 2;
/// User/consent layer denied it.
pub const PF_CONSENT_DENIED: i32 = 3;
/// Device has no such hardware.
pub const PF_HARDWARE_ABSENT: i32 = 4;

/// `pf_query`: granted.
pub const PF_GRANTED: i32 = 0;
/// `pf_query`: denied.
pub const PF_DENIED: i32 = 1;
/// `pf_query`: would prompt.
pub const PF_PROMPT: i32 = 2;

/// `pf_rumble_pulse`: motor present + enabled, would actuate.
pub const PF_RUMBLE_FIRED: i32 = 0;
/// `pf_rumble_pulse`: no motor on this device.
pub const PF_RUMBLE_NOOP_ABSENT: i32 = 1;
/// `pf_rumble_pulse`: motor present but haptics suppressed.
pub const PF_RUMBLE_NOOP_SUPPRESSED: i32 = 2;

fn cap_err_code(e: pf::CapError) -> i32 {
    e.code() as i32
}

/// Borrow a C string as `&str`; returns `None` for NULL / non-UTF-8.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// Open a session using the environment (see `pocketforge::connect`). Returns NULL on error.
///
/// # Safety
/// The returned pointer must be freed once with [`pf_free`].
#[no_mangle]
pub extern "C" fn pf_connect() -> *mut PfSession {
    match pf::connect() {
        Ok(pf) => Box::into_raw(Box::new(PfSession { pf })),
        Err(e) => {
            eprintln!("libpocketforge: pf_connect failed: {e}");
            ptr::null_mut()
        }
    }
}

/// Open a session over the v0 in-process backend for an explicit `capabilities.toml` path.
/// Returns NULL on error.
///
/// # Safety
/// `path` must be a valid NUL-terminated string; the result must be freed with [`pf_free`].
#[no_mangle]
pub unsafe extern "C" fn pf_connect_descriptor(path: *const c_char) -> *mut PfSession {
    let Some(path) = cstr(path) else { return ptr::null_mut() };
    match Descriptor::load(path) {
        Ok(d) => Box::into_raw(Box::new(PfSession { pf: Pf::in_process(d) })),
        Err(e) => {
            eprintln!("libpocketforge: pf_connect_descriptor failed: {e}");
            ptr::null_mut()
        }
    }
}

/// Free a session. NULL is ignored.
///
/// # Safety
/// `s` must be a pointer from `pf_connect*` not already freed.
#[no_mangle]
pub unsafe extern "C" fn pf_free(s: *mut PfSession) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

/// Two-stage capability detection. On a NULL/invalid session returns `{0,0}`.
///
/// # Safety
/// `s` from `pf_connect*`; `name` a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn pf_has_capability(s: *const PfSession, name: *const c_char) -> PfPresence {
    let (Some(sess), Some(name)) = (s.as_ref(), cstr(name)) else {
        return PfPresence { api: 0, hardware: 0 };
    };
    // by-name presence: api=1 if known to the platform, hardware from the backend probe.
    let api = pf::backend::is_known(name) as i32;
    let hardware = sess.pf.backend().is_present(name) as i32;
    PfPresence { api, hardware }
}

/// 1 if the capability is present (hardware), else 0.
///
/// # Safety
/// See [`pf_has_capability`].
#[no_mangle]
pub unsafe extern "C" fn pf_is_present(s: *const PfSession, name: *const c_char) -> i32 {
    match (s.as_ref(), cstr(name)) {
        (Some(sess), Some(name)) => sess.pf.backend().is_present(name) as i32,
        _ => 0,
    }
}

/// 1 if currently granted (present AND policy-allowed), else 0.
///
/// # Safety
/// See [`pf_has_capability`].
#[no_mangle]
pub unsafe extern "C" fn pf_is_granted(s: *const PfSession, name: *const c_char) -> i32 {
    match (s.as_ref(), cstr(name)) {
        (Some(sess), Some(name)) => sess.pf.backend().is_granted(name) as i32,
        _ => 0,
    }
}

/// Side-effect-free permission query: `PF_GRANTED` / `PF_DENIED` / `PF_PROMPT`. Invalid args
/// return `PF_DENIED` (fail-closed).
///
/// # Safety
/// See [`pf_has_capability`].
#[no_mangle]
pub unsafe extern "C" fn pf_query(s: *const PfSession, name: *const c_char) -> i32 {
    let (Some(sess), Some(name)) = (s.as_ref(), cstr(name)) else { return PF_DENIED };
    match sess.pf.backend().query(name) {
        pf::PermissionState::Granted => PF_GRANTED,
        pf::PermissionState::Denied => PF_DENIED,
        pf::PermissionState::Prompt => PF_PROMPT,
    }
}

/// Acquire a capability by name. Returns `PF_OK` or the four-way taxonomy code. Cosmetic caps
/// (`vibration`/`rumble`/`leds`) always return `PF_OK`. Invalid args return `PF_UNSUPPORTED`.
///
/// # Safety
/// See [`pf_has_capability`].
#[no_mangle]
pub unsafe extern "C" fn pf_acquire(s: *const PfSession, name: *const c_char) -> i32 {
    let (Some(sess), Some(name)) = (s.as_ref(), cstr(name)) else { return PF_UNSUPPORTED };
    match sess.pf.acquire_by_name(name) {
        Ok(()) => PF_OK,
        Err(e) => cap_err_code(e),
    }
}

/// Pulse the rumble motor for `ms` ms — the unified no-op shape. Returns a `PF_RUMBLE_*` code
/// (never fails). Invalid session returns `PF_RUMBLE_NOOP_ABSENT`.
///
/// # Safety
/// `s` from `pf_connect*`.
#[no_mangle]
pub unsafe extern "C" fn pf_rumble_pulse(s: *const PfSession, ms: u32) -> i32 {
    let Some(sess) = s.as_ref() else { return PF_RUMBLE_NOOP_ABSENT };
    match sess.pf.backend().rumble_pulse(ms) {
        pf::RumbleStatus::Fired => PF_RUMBLE_FIRED,
        pf::RumbleStatus::NoopAbsent => PF_RUMBLE_NOOP_ABSENT,
        pf::RumbleStatus::NoopSuppressed => PF_RUMBLE_NOOP_SUPPRESSED,
    }
}

/// Fill `buf[0..len]` with CSPRNG bytes (ungated entropy). Returns 0 on success, -1 on error.
///
/// # Safety
/// `buf` must point to at least `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn pf_entropy_fill(s: *const PfSession, buf: *mut u8, len: usize) -> i32 {
    let Some(sess) = s.as_ref() else { return -1 };
    if buf.is_null() || len == 0 {
        return -1;
    }
    let slice = std::slice::from_raw_parts_mut(buf, len);
    match sess.pf.acquire::<pf::Entropy>() {
        Ok(h) => match h.fill(slice) {
            Ok(()) => 0,
            Err(_) => -1,
        },
        Err(_) => -1,
    }
}

/// The PFW1 wire-protocol version this library speaks.
#[no_mangle]
pub extern "C" fn pf_wire_version() -> u32 {
    pf::pf_wire::WIRE_VERSION
}

/// A static human-readable string for a `pf_acquire` status code. Never NULL.
#[no_mangle]
pub extern "C" fn pf_strerror(status: i32) -> *const c_char {
    let s: &CStr = match status {
        PF_OK => c"ok",
        PF_UNSUPPORTED => c"unsupported",
        PF_POLICY_BLOCKED => c"policy-blocked",
        PF_CONSENT_DENIED => c"consent-denied",
        PF_HARDWARE_ABSENT => c"hardware-absent",
        _ => c"unknown",
    };
    s.as_ptr()
}
