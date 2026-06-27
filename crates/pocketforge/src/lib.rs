//! # `pocketforge` — the app-facing capability facade
//!
//! A `getSystemService`-style facade over a **capability broker**, so an app asks for a
//! capability by type and gets back a handle or a typed reason — never raw `/dev/*`:
//!
//! ```no_run
//! use pocketforge::{connect, Vibration, Location, Imu, PermissionState, RumbleStatus};
//!
//! let pf = connect()?;                          // descriptor + backend chosen by env
//! let vib = pf.acquire::<Vibration>()?;         // cosmetic: always Ok, degrades at the handle
//! let _ = vib.pulse(40);                        // Fired | NoopAbsent | NoopSuppressed
//!
//! match pf.query::<Location>() {                // side-effect-free: Granted | Denied | Prompt
//!     PermissionState::Prompt => { /* the supervisor would draw consent (E3) */ }
//!     _ => {}
//! }
//!
//! if pf.has_capability::<Imu>().present() {      // two-stage: API present AND hardware present
//!     let imu = pf.acquire::<Imu>()?;           // HardwareAbsent on the base Pro
//!     let _pose = imu.read_pose();
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## The backend swap (the load-bearing property)
//!
//! The same facade sits over two interchangeable backends ([`backends`]):
//! [`InProcessBackend`](backends::InProcessBackend) (v0, cooperative, direct) and
//! [`BrokerClientBackend`](backends::BrokerClientBackend) (out-of-process, PFW1 over a Unix
//! socket to the broker). Choosing one is a constructor/env decision — **never** an app-source
//! change. That is what lets E6 (`pf-hwprobe`) run unchanged on both, "surviving the runtime
//! fork."
//!
//! ## Honesty (R-A): contract now, enforce later
//!
//! The v0 in-process backend is a **cooperative** library — it proves the capability/permission
//! *contract*, ergonomics, and descriptor-honest graceful missing-hardware degradation, NOT
//! confinement. Real enforcement (default-deny vs. hostile apps, unforgeable routed handles) is
//! the out-of-process broker (`.3`) on the Phase-2 substrate; INPUT is the one v0-enforceable
//! cap (`uinput`+`EVIOCGRAB`, `.6`). See `README.md` + `wire/WIRE-PROTOCOL.md` §7.

pub mod backend;
pub mod backends;
pub mod capability;
pub mod descriptor;
pub mod error;
pub mod input;
pub mod server;

use std::sync::Arc;

pub use backend::{Backend, Pose, PoseDelta, RumbleStatus};
pub use capability::{
    Accelerometer, Audio, AudioHandle, Capability, CapabilityPresence, Entropy, EntropyHandle,
    Gyroscope, Imu, Input, InputHandle, Leds, LedsHandle, Location, LocationHandle, Magnetometer,
    Settings, SettingsHandle, SensorHandle, Vibration, VibrationHandle,
};
pub use descriptor::Descriptor;
pub use error::{CapError, ConnectError, PermissionState};
pub use input::{InputAction, InputMap};

// Re-export the wire crate so reimplementors + the C ABI crate share one definition.
pub use pf_wire;

/// A live capability session — the object `acquire`/`query`/`has_capability` hang off. Cheap to
/// hold; it owns a shared descriptor + a shared backend (the swap seam).
pub struct Pf {
    descriptor: Arc<Descriptor>,
    backend: Arc<dyn Backend>,
}

impl Pf {
    /// Build a session over an explicit descriptor + backend (the general constructor).
    pub fn with_backend(descriptor: Arc<Descriptor>, backend: Arc<dyn Backend>) -> Pf {
        Pf { descriptor, backend }
    }

    /// Build a session over the **v0 in-process backend** for a descriptor.
    pub fn in_process(descriptor: Descriptor) -> Pf {
        let d = Arc::new(descriptor);
        let backend = backends::InProcessBackend::shared(d.clone());
        Pf { descriptor: d, backend }
    }

    /// Build a session over an already-shared in-process backend (so a test/control plane can
    /// drive `set_consent`/`set_pose` on the same backend the session observes).
    pub fn over_in_process(backend: Arc<backends::InProcessBackend>) -> Pf {
        let d = backend.descriptor().clone();
        Pf { descriptor: d, backend }
    }

    /// Build a session over the **out-of-process broker** at `sock_path`. The descriptor is the
    /// read-only file the app holds for structure (input rows, led count); the broker arbitrates.
    pub fn via_broker(
        descriptor: Arc<Descriptor>,
        sock_path: impl AsRef<std::path::Path>,
    ) -> Result<Pf, ConnectError> {
        let backend = backends::BrokerClientBackend::connect(sock_path).map_err(ConnectError::Broker)?;
        Ok(Pf { descriptor, backend: Arc::new(backend) })
    }

    /// The descriptor this session reads structure from.
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// The backend (arbitration) as a trait object.
    pub fn backend(&self) -> &dyn Backend {
        &*self.backend
    }

    /// A shared clone of the backend (handles hold this to call back).
    pub fn backend_arc(&self) -> Arc<dyn Backend> {
        self.backend.clone()
    }

    /// Acquire a capability handle, or the four-way typed error. (Cosmetic caps never error.)
    pub fn acquire<C: Capability>(&self) -> Result<C::Handle, CapError> {
        C::acquire(self)
    }

    /// The side-effect-free Permissions-API `query()` for a capability.
    pub fn query<C: Capability>(&self) -> PermissionState {
        self.backend.query(C::NAME)
    }

    /// Two-stage detection: API-present (the type is compiled in) vs hardware-present.
    pub fn has_capability<C: Capability>(&self) -> CapabilityPresence {
        CapabilityPresence { api: true, hardware: self.backend.is_present(C::NAME) }
    }

    /// Convenience: is this capability currently granted (present AND policy-allowed)?
    pub fn is_granted<C: Capability>(&self) -> bool {
        self.backend.is_granted(C::NAME)
    }

    /// Acquire by capability **name** (the dynamic / C-ABI path). Encodes the same cosmetic
    /// no-op tier as the typed `acquire::<Vibration>()` — `vibration`/`rumble`/`leds` never
    /// error; everything else goes through the canonical arbitration. One definition shared by
    /// the typed facade and the C ABI so they cannot drift.
    pub fn acquire_by_name(&self, name: &str) -> Result<(), CapError> {
        let n = name.to_ascii_lowercase();
        if matches!(n.as_str(), "vibration" | "rumble" | "leds") {
            return Ok(());
        }
        self.backend.acquire(&n)
    }
}

/// Open a capability session, choosing the descriptor + backend from the environment:
///
/// * **Descriptor:** `PF_DESCRIPTOR` (a `capabilities.toml` path) wins; else `PF_DEVICE_ID` +
///   `PF_PLATFORM` resolve `<PF_PLATFORM>/devices/<id>/capabilities.toml`.
/// * **Backend:** if `PF_BROKER_SOCK` is set, the out-of-process broker client; else the v0
///   in-process backend. This env switch is the backend swap — no app-source change.
pub fn connect() -> Result<Pf, ConnectError> {
    let descriptor = load_descriptor_from_env()?;
    let d = Arc::new(descriptor);
    if let Some(sock) = std::env::var_os("PF_BROKER_SOCK") {
        Pf::via_broker(d, sock)
    } else {
        let backend = backends::InProcessBackend::shared(d.clone());
        Ok(Pf { descriptor: d, backend })
    }
}

fn load_descriptor_from_env() -> Result<Descriptor, ConnectError> {
    if let Some(path) = std::env::var_os("PF_DESCRIPTOR") {
        return Descriptor::load(path).map_err(ConnectError::from);
    }
    if let (Some(id), Some(root)) = (std::env::var_os("PF_DEVICE_ID"), std::env::var_os("PF_PLATFORM")) {
        let mut p = std::path::PathBuf::from(root);
        p.push("devices");
        p.push(id);
        p.push("capabilities.toml");
        return Descriptor::load(p).map_err(ConnectError::from);
    }
    Err(ConnectError::NoDescriptor(
        "set PF_DESCRIPTOR=<capabilities.toml> or PF_DEVICE_ID + PF_PLATFORM".into(),
    ))
}
