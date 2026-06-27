//! The typed capability surface: zero-sized **marker types** (`Vibration`, `Imu`, `Location`,
//! …) implementing [`Capability`], each with a typed **handle**. This is what makes
//! `pf.acquire::<Vibration>()` and `pf.query::<Location>()` work, and what encodes the
//! cosmetic **no-op tier** in the type system: cosmetic caps (`Vibration`, `Leds`) `acquire`
//! infallibly and degrade at the handle; hardware caps return the four-way [`CapError`].

use std::io::Read;
use std::sync::Arc;

use crate::backend::{Backend, Pose, RumbleStatus};
use crate::error::CapError;
use crate::input::InputMap;
use crate::Pf;

/// A capability the runtime can vend. Implemented by the zero-sized marker types below.
pub trait Capability {
    /// The wire/descriptor capability name (e.g. `"vibration"`).
    const NAME: &'static str;
    /// The typed handle `acquire` yields.
    type Handle;
    /// Resolve an acquisition against a session: a handle, or the four-way typed error.
    /// (Cosmetic caps never error — they return a degraded handle.)
    fn acquire(pf: &Pf) -> Result<Self::Handle, CapError>;
}

/// Two-stage capability detection (briefing: "API present != hardware present").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityPresence {
    /// The capability *type* is compiled into the runtime (always true for a known marker).
    pub api: bool,
    /// The descriptor + live probe back it with real hardware on THIS device.
    pub hardware: bool,
}

impl CapabilityPresence {
    /// Both stages satisfied — the app can use it for real.
    pub fn present(self) -> bool {
        self.api && self.hardware
    }
}

// ---------------------------------------------------------------------------
// Marker types.
// ---------------------------------------------------------------------------

/// Gamepad / button input (action-mapped). Always present (every device has inputs).
pub struct Input;
/// Haptic rumble — **cosmetic no-op tier** (a133 has no motor; E4 can suppress).
pub struct Vibration;
/// 6-axis IMU (accel+gyro). Absent on the base Pro ⇒ `HardwareAbsent`.
pub struct Imu;
/// Accelerometer. Absent on the base Pro ⇒ `HardwareAbsent`.
pub struct Accelerometer;
/// Gyroscope. Absent on the base Pro ⇒ `HardwareAbsent`.
pub struct Gyroscope;
/// Magnetometer. Absent on both starter devices ⇒ `HardwareAbsent`.
pub struct Magnetometer;
/// CSPRNG entropy — the deliberate **ungated** capability.
pub struct Entropy;
/// GNSS location — privacy **dangerous tier**, default-deny ⇒ `ConsentDenied` in v0.
pub struct Location;
/// RGB LED array — **cosmetic no-op tier** (degrades to a 0-count handle if absent).
pub struct Leds;
/// Audio routing — a platform service (present on every device).
pub struct Audio;
/// User/app settings — a platform service (present on every device).
pub struct Settings;

// ---------------------------------------------------------------------------
// Handles.
// ---------------------------------------------------------------------------

/// Handle for the input capability: the descriptor-derived action map. The per-event hot path
/// (the shared `uinput` fd) is delivered by child `.6`; this handle is the RPC-free map.
pub struct InputHandle {
    map: InputMap,
}

impl InputHandle {
    /// The descriptor-derived action map (named-action resolution + all bindable controls).
    pub fn map(&self) -> &InputMap {
        &self.map
    }
}

/// Handle for haptics — the unified no-op shape. `pulse` ALWAYS returns a status, never fails.
pub struct VibrationHandle {
    backend: Arc<dyn Backend>,
    /// Whether the descriptor advertises a real motor (false ⇒ every pulse is `NoopAbsent`).
    pub has_motor: bool,
}

impl VibrationHandle {
    /// Pulse for `ms` ms. Returns the typed no-op shape (`Fired` / `NoopAbsent` /
    /// `NoopSuppressed`) — the app does NOT special-case absence or the haptics preference.
    pub fn pulse(&self, ms: u32) -> RumbleStatus {
        self.backend.rumble_pulse(ms)
    }
}

/// Handle for IMU / individual inertial sensors. Wraps the `.4`
/// [`SensorManager`](crate::managers::SensorManager): the pose plus the derived device/chip-frame
/// accelerometer + gyroscope channels (via the single physical model + the descriptor
/// `mount_matrix`). `HardwareAbsent` on the base Pro, never a crash.
pub struct SensorHandle {
    mgr: crate::managers::SensorManager,
}

impl SensorHandle {
    /// Read the current pose (orientation in degrees, angular velocity in deg/s).
    pub fn read_pose(&self) -> Result<Pose, CapError> {
        self.mgr.read_pose()
    }

    /// The accelerometer reading in the **device frame** (m/s²) — gravity reaction from the pose.
    pub fn read_accel(&self) -> Result<[f64; 3], CapError> {
        self.mgr.read_accel()
    }

    /// The gyroscope reading in the **device frame** (rad/s).
    pub fn read_gyro(&self) -> Result<[f64; 3], CapError> {
        self.mgr.read_gyro()
    }

    /// The accelerometer in the **chip frame** (what the raw IIO node holds, pre-mount-matrix).
    pub fn read_chip_accel(&self) -> Result<[f64; 3], CapError> {
        self.mgr.read_chip_accel()
    }

    /// The chip→device mount matrix this device applies (identity unless the descriptor remaps).
    pub fn mount_matrix(&self) -> &crate::physical_model::Mat3 {
        self.mgr.mount_matrix()
    }
}

/// Handle for the ungated entropy source.
pub struct EntropyHandle {
    _private: (),
}

impl EntropyHandle {
    /// Fill `buf` with cryptographically-strong random bytes from the OS CSPRNG.
    /// (v0 reads `/dev/urandom` directly — entropy is ungated; the broker never rate-limits it.
    /// The boot-seed-quality probe is a separate E1/R-G follow-up, not this path.)
    pub fn fill(&self, buf: &mut [u8]) -> std::io::Result<()> {
        let mut f = std::fs::File::open("/dev/urandom")?;
        f.read_exact(buf)
    }
}

/// Handle for GNSS location (only vended when granted; default-deny in v0 means this is rarely
/// reachable until E3's consent UI exists). `read` is a placeholder until child `.4`.
pub struct LocationHandle {
    _private: (),
}

/// Handle for the RGB LED array — cosmetic no-op tier. `count` is 0 when absent.
pub struct LedsHandle {
    /// Number of addressable LEDs (descriptor-derived; 0 ⇒ degraded no-op handle).
    pub count: u32,
}

/// Handle for audio routing (platform service; full routing is child `.4`).
pub struct AudioHandle {
    _private: (),
}

/// Handle for user/app settings (platform service; full surface is child `.4` / E4).
pub struct SettingsHandle {
    _private: (),
}

// ---------------------------------------------------------------------------
// Capability impls. Hardware caps gate through the canonical `backend.acquire`; cosmetic caps
// (Vibration, Leds) never error.
// ---------------------------------------------------------------------------

impl Capability for Input {
    const NAME: &'static str = "input";
    type Handle = InputHandle;
    fn acquire(pf: &Pf) -> Result<InputHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(InputHandle { map: InputMap::from_descriptor(pf.descriptor()) })
    }
}

impl Capability for Vibration {
    const NAME: &'static str = "vibration";
    type Handle = VibrationHandle;
    fn acquire(pf: &Pf) -> Result<VibrationHandle, CapError> {
        // Cosmetic no-op tier: ALWAYS Ok; absence/suppression surface from `pulse`.
        Ok(VibrationHandle {
            backend: pf.backend_arc(),
            has_motor: pf.backend().is_present("rumble"),
        })
    }
}

impl Capability for Imu {
    const NAME: &'static str = "imu";
    type Handle = SensorHandle;
    fn acquire(pf: &Pf) -> Result<SensorHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(SensorHandle { mgr: pf.sensors() })
    }
}

impl Capability for Accelerometer {
    const NAME: &'static str = "accelerometer";
    type Handle = SensorHandle;
    fn acquire(pf: &Pf) -> Result<SensorHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(SensorHandle { mgr: pf.sensors() })
    }
}

impl Capability for Gyroscope {
    const NAME: &'static str = "gyroscope";
    type Handle = SensorHandle;
    fn acquire(pf: &Pf) -> Result<SensorHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(SensorHandle { mgr: pf.sensors() })
    }
}

impl Capability for Magnetometer {
    const NAME: &'static str = "magnetometer";
    type Handle = SensorHandle;
    fn acquire(pf: &Pf) -> Result<SensorHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(SensorHandle { mgr: pf.sensors() })
    }
}

impl Capability for Entropy {
    const NAME: &'static str = "entropy";
    type Handle = EntropyHandle;
    fn acquire(pf: &Pf) -> Result<EntropyHandle, CapError> {
        pf.backend().acquire(Self::NAME)?; // ungated ⇒ Ok
        Ok(EntropyHandle { _private: () })
    }
}

impl Capability for Location {
    const NAME: &'static str = "location";
    type Handle = LocationHandle;
    fn acquire(pf: &Pf) -> Result<LocationHandle, CapError> {
        pf.backend().acquire(Self::NAME)?; // default-deny ⇒ ConsentDenied (or HardwareAbsent)
        Ok(LocationHandle { _private: () })
    }
}

impl Capability for Leds {
    const NAME: &'static str = "leds";
    type Handle = LedsHandle;
    fn acquire(pf: &Pf) -> Result<LedsHandle, CapError> {
        // Cosmetic no-op tier: ALWAYS Ok; a 0 count is the degraded handle.
        Ok(LedsHandle { count: pf.descriptor().led_count() })
    }
}

impl Capability for Audio {
    const NAME: &'static str = "audio";
    type Handle = AudioHandle;
    fn acquire(pf: &Pf) -> Result<AudioHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(AudioHandle { _private: () })
    }
}

impl Capability for Settings {
    const NAME: &'static str = "settings";
    type Handle = SettingsHandle;
    fn acquire(pf: &Pf) -> Result<SettingsHandle, CapError> {
        pf.backend().acquire(Self::NAME)?;
        Ok(SettingsHandle { _private: () })
    }
}
