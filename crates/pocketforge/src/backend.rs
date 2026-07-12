//! The **backend trait** — the swap seam. The facade ([`crate::Pf`]) is generic over a
//! `dyn Backend`; the two implementations behind the IDENTICAL facade are:
//!
//! * [`crate::backends::InProcessBackend`] — the v0 in-process backend (direct
//!   descriptor-derived arbitration; the Rust port of the sim's `broker_stub.py`).
//! * [`crate::backends::BrokerClientBackend`] — the out-of-process client that speaks PFW1
//!   over a Unix socket to the broker (`.3` / reference [`crate::server`]).
//!
//! Swapping is a constructor/env choice ([`crate::connect`]), NEVER an app-source change —
//! that is the load-bearing "survives the runtime fork" property the epic demands.
//!
//! The **canonical arbitration semantics** live in this module's free functions
//! ([`acquire_decision`], [`query_decision`], …) so both the in-process backend AND the
//! reference server compute identically — the swap cannot drift in behavior.

use std::sync::mpsc::Receiver;

use crate::error::{CapError, PermissionState};
pub use pf_wire::RumbleStatus;

// Re-export the preference value type so the observer payload has one definition (the E4 data
// layer's), and callers of `subscribe_preference` / `preference_scalar` do not reach into
// `pf_prefs` directly through the facade.
pub use pf_prefs::PrefValue;

/// A rigid-body pose in human/UI units (degrees, deg/s) — the shape `set_pose`/`get_pose`
/// exchange. (The full integrating physical model from the sim's `physical_model.py` lands in
/// `.4`; v0 carries the latest set values, which is enough to prove the contract + the
/// hardware-absent path.)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub wx: f64,
    pub wy: f64,
    pub wz: f64,
}

impl Pose {
    /// The fields in canonical wire order (`yaw,pitch,roll,x,y,z,wx,wy,wz`).
    fn fields(&self) -> [f64; 9] {
        [self.yaw, self.pitch, self.roll, self.x, self.y, self.z, self.wx, self.wy, self.wz]
    }

    /// Encode to 72 bytes (9× f64 little-endian) — the PFW1 pose payload.
    pub fn to_bytes(&self) -> [u8; 72] {
        let mut out = [0u8; 72];
        for (i, v) in self.fields().iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Decode from the PFW1 pose payload (exactly 72 bytes).
    pub fn from_bytes(b: &[u8]) -> Option<Pose> {
        if b.len() != 72 {
            return None;
        }
        let mut f = [0f64; 9];
        for (i, slot) in f.iter_mut().enumerate() {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&b[i * 8..i * 8 + 8]);
            *slot = f64::from_le_bytes(buf);
        }
        Some(Pose {
            yaw: f[0], pitch: f[1], roll: f[2],
            x: f[3], y: f[4], z: f[5],
            wx: f[6], wy: f[7], wz: f[8],
        })
    }

    /// Apply a partial delta in place (None = unchanged).
    pub fn apply(&mut self, d: &PoseDelta) {
        macro_rules! set { ($field:ident) => { if let Some(v) = d.$field { self.$field = v; } } }
        set!(yaw); set!(pitch); set!(roll);
        set!(x); set!(y); set!(z);
        set!(wx); set!(wy); set!(wz);
    }
}

/// A partial pose update — `None` fields are left unchanged (AVD `setPhysicalModel` style).
#[derive(Debug, Clone, Copy, Default)]
pub struct PoseDelta {
    pub yaw: Option<f64>,
    pub pitch: Option<f64>,
    pub roll: Option<f64>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    pub wx: Option<f64>,
    pub wy: Option<f64>,
    pub wz: Option<f64>,
}

/// The arbitration interface a backend implements. Structural reads (input rows, led count,
/// identity) are NOT here — those come from the descriptor the client holds directly; the
/// backend only arbitrates *access, permission, and actuation*.
pub trait Backend: Send + Sync {
    /// Is the capability present (descriptor + live probe)? Side-effect-free.
    fn is_present(&self, name: &str) -> bool;
    /// Present AND policy-allowed (no consent prompt outstanding). Side-effect-free.
    fn is_granted(&self, name: &str) -> bool;
    /// Permissions-API `query()`: Granted | Denied | Prompt. Side-effect-free.
    fn query(&self, name: &str) -> PermissionState;
    /// Acquire arbitration: `Ok` if granted, else the four-way taxonomy. (Cosmetic no-op caps
    /// — rumble/leds — are resolved at the capability layer and do not call this.)
    fn acquire(&self, name: &str) -> Result<(), CapError>;
    /// Pulse the rumble actuator for `ms` ms — the unified no-op shape (never fails).
    fn rumble_pulse(&self, ms: u32) -> RumbleStatus;
    /// Read the current IMU pose, or `HardwareAbsent` if the descriptor has no IMU.
    fn get_pose(&self) -> Result<Pose, CapError>;
    /// Set the IMU pose absolutely (the injection-as-API seam), returning the new pose, or
    /// `HardwareAbsent` if the descriptor has no IMU. (Partial-delta convenience lives on
    /// [`crate::backends::InProcessBackend::set_pose_delta`].)
    fn set_pose(&self, pose: Pose) -> Result<Pose, CapError>;
    /// Cooperative get of a capability's stored value (empty = unset), gated by presence+policy.
    fn get_capability(&self, name: &str) -> Result<Vec<u8>, CapError>;
    /// Cooperative set of a capability's value, gated by presence+policy.
    fn set_capability(&self, name: &str, value: &[u8]) -> Result<(), CapError>;
    /// Read an accessibility/user preference bool (E4 seam; default if unset).
    fn preference_bool(&self, name: &str, default: bool) -> bool;
    /// Set an accessibility/user preference bool (fires the query() change-event where relevant).
    fn set_preference_bool(&self, name: &str, value: bool);

    /// Read an accessibility/user preference SCALAR (E4; `brightness` today), or `default` if the
    /// backend has no store-backed value. Defaulted so a backend without scalar preferences (the
    /// v0 broker client, which has no preference wire op yet) stays honest — it returns the
    /// caller's default rather than fabricating one. The in-process backend overrides this to read
    /// the persistent store; `EnforcingBackend` delegates to its inner backend.
    fn preference_scalar(&self, name: &str, default: i64) -> i64 {
        let _ = name;
        default
    }

    /// Subscribe to `PrefsDidChange` for a preference `name`, mirroring the query() change-event
    /// shape ([`crate::backends::InProcessBackend::subscribe`]). Returns `Some(Receiver)` on a
    /// backend that can observe preferences in-process (the v0 in-process backend) and `None` on
    /// one that cannot yet (the broker client — preferences are not a wire op in v0; R-A honesty:
    /// name the gap, do not fake an observer that never fires). The receiver yields the new
    /// effective [`PrefValue`] on every write path (CLI reload, control plane, future UI).
    fn subscribe_preference(&self, name: &str) -> Option<Receiver<PrefValue>> {
        let _ = name;
        None
    }
}

// ---------------------------------------------------------------------------
// Canonical arbitration policy — shared by the in-process backend and the reference
// server so the backend swap cannot change behavior. (The REAL default-deny-vs-hostile
// enforcement is `.3` on the Phase-2 substrate; this is the v0 cooperative contract.)
// ---------------------------------------------------------------------------

/// Privacy-sensitive capabilities that default-deny (briefing §A "dangerous" tier). v0 has no
/// consent UI (E3), so these surface as `ConsentDenied`/`Prompt`.
pub const DEFAULT_DENY: &[&str] = &["location", "gnss"];

/// Capability names the platform/runtime supports (the API surface). Distinguishes
/// `HardwareAbsent` (known cap, no hardware) from `Unsupported` (no such cap at all).
pub const KNOWN_CAPS: &[&str] = &[
    "input",
    "vibration",
    "rumble",
    "imu",
    "accelerometer",
    "gyroscope",
    "magnetometer",
    "entropy",
    "location",
    "gnss",
    "audio",
    "settings",
    "leds",
];

/// True if `name` is a capability the platform knows about at all.
pub fn is_known(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    KNOWN_CAPS.iter().any(|k| *k == n)
}

/// True if `name` is in the default-deny privacy tier.
pub fn is_default_deny(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    DEFAULT_DENY.iter().any(|k| *k == n)
}

/// Canonical grant decision (present AND not default-deny) — matches `broker_stub.is_granted`.
pub fn granted_decision(present: bool, name: &str) -> bool {
    present && !is_default_deny(name)
}

/// Canonical `query()` decision: absent ⇒ Denied; present default-deny ⇒ Prompt; else Granted.
pub fn query_decision(present: bool, name: &str) -> PermissionState {
    if !present {
        PermissionState::Denied
    } else if is_default_deny(name) {
        PermissionState::Prompt
    } else {
        PermissionState::Granted
    }
}

/// Canonical `acquire()` decision. Absent + known ⇒ HardwareAbsent; absent + unknown ⇒
/// Unsupported; present default-deny ⇒ ConsentDenied; else Ok.
pub fn acquire_decision(present: bool, name: &str) -> Result<(), CapError> {
    if !present {
        return Err(if is_known(name) {
            CapError::HardwareAbsent
        } else {
            CapError::Unsupported
        });
    }
    if is_default_deny(name) {
        return Err(CapError::ConsentDenied);
    }
    Ok(())
}
