//! The four-way typed-error taxonomy + the side-effect-free permission state.
//!
//! These are the heart of the capability contract (briefing §A "steal these specifics"):
//! a capability request never returns an untyped failure — it returns exactly one of four
//! reasons, so an app degrades deliberately instead of crashing. The cosmetic **no-op tier**
//! (rumble/leds) is expressed differently: those `acquire` calls return `Ok(handle)` and the
//! handle's operation returns a [`crate::RumbleStatus`] — see [`crate::capability`].

use pf_wire::{Permission, Status};

/// Why acquiring (or operating on) a capability failed — the four-way taxonomy.
///
/// Mirrors `pf_wire::Status` (minus `Ok`) so the wire and the Rust type are the same shape;
/// the C ABI re-exports the same integer values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// The platform/runtime has no such capability type at all (not just absent hardware).
    Unsupported,
    /// Refused by static policy — the `app.toml` `use=[]` graph or a protection tier forbids
    /// it. (Enforced by the supervisor/broker in `.3`; in the v0 cooperative facade this is
    /// the "declared-but-blocked" case.)
    PolicyBlocked,
    /// The user/consent layer denied it (a default-deny privacy cap with no granted consent).
    /// v0 has no consent UI yet (E3), so default-deny caps surface here.
    ConsentDenied,
    /// The device descriptor advertises no such hardware (the graceful-degradation case).
    HardwareAbsent,
}

impl CapError {
    /// The stable integer this maps to on the wire / C ABI (matches `pf_wire::Status`).
    pub fn code(self) -> u8 {
        self.status() as u8
    }

    /// The corresponding non-`Ok` wire status.
    pub fn status(self) -> Status {
        match self {
            CapError::Unsupported => Status::Unsupported,
            CapError::PolicyBlocked => Status::PolicyBlocked,
            CapError::ConsentDenied => Status::ConsentDenied,
            CapError::HardwareAbsent => Status::HardwareAbsent,
        }
    }

    /// Build from a wire status; `Ok` is not an error and yields `None`.
    pub fn from_status(s: Status) -> Option<CapError> {
        match s {
            Status::Ok => None,
            Status::Unsupported => Some(CapError::Unsupported),
            Status::PolicyBlocked => Some(CapError::PolicyBlocked),
            Status::ConsentDenied => Some(CapError::ConsentDenied),
            Status::HardwareAbsent => Some(CapError::HardwareAbsent),
        }
    }
}

impl std::fmt::Display for CapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CapError::Unsupported => "unsupported (no such capability on this platform)",
            CapError::PolicyBlocked => "policy-blocked (not granted by manifest/tier)",
            CapError::ConsentDenied => "consent-denied (user has not granted this)",
            CapError::HardwareAbsent => "hardware-absent (device has no such hardware)",
        };
        f.write_str(s)
    }
}

impl std::error::Error for CapError {}

/// The Permissions-API `query()` result — **side-effect-free**: it never prompts, acquires,
/// or mutates. `Prompt` means "asking would show a consent UI" (a default-deny cap not yet
/// decided); `Granted`/`Denied` are settled states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    Prompt,
}

impl PermissionState {
    /// Map to/from the wire enum.
    pub fn to_wire(self) -> Permission {
        match self {
            PermissionState::Granted => Permission::Granted,
            PermissionState::Denied => Permission::Denied,
            PermissionState::Prompt => Permission::Prompt,
        }
    }

    /// Build from the wire enum.
    pub fn from_wire(p: Permission) -> PermissionState {
        match p {
            Permission::Granted => PermissionState::Granted,
            Permission::Denied => PermissionState::Denied,
            Permission::Prompt => PermissionState::Prompt,
        }
    }
}

/// Errors from [`crate::connect`] / constructing a session (distinct from capability errors).
#[derive(Debug)]
pub enum ConnectError {
    /// No descriptor could be located (missing `PF_DESCRIPTOR` / `PF_DEVICE_ID` + platform root).
    NoDescriptor(String),
    /// The descriptor failed to load or parse.
    Descriptor(crate::descriptor::DescriptorError),
    /// The broker socket could not be reached (out-of-process backend).
    Broker(std::io::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoDescriptor(d) => write!(f, "no device descriptor: {d}"),
            ConnectError::Descriptor(e) => write!(f, "descriptor: {e}"),
            ConnectError::Broker(e) => write!(f, "broker connect: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<crate::descriptor::DescriptorError> for ConnectError {
    fn from(e: crate::descriptor::DescriptorError) -> Self {
        ConnectError::Descriptor(e)
    }
}
