//! The **vibration manager** — `FF_RUMBLE` via `pwm-vibrator`, expressed as the unified no-op
//! shape. The base Pro (a133) has no motor ⇒ every pulse is `NoopAbsent`; the Pro S (a523) has a
//! `pwm-vibrator` ⇒ `Fired`, UNLESS the user disabled haptics (E4), which makes the SAME call
//! `NoopSuppressed` via the SAME path. **The E4 accessibility enforcement point lands here, AT
//! the primitive**: the recommended `pulse` honors the global `hapticsEnabled` preference without
//! any vibrate permission — preference-suppression and missing-hardware are one no-op shape, so
//! the app never special-cases either. (The backend computes the status so it stays identical
//! across the backend swap; this manager is the device-agnostic surface over it.)

use std::sync::Arc;

use crate::backend::{Backend, RumbleStatus};

use super::{reconcile_presence, HardwareProbe};

/// One device-agnostic haptics object (cosmetic no-op tier — never errors).
pub struct VibrationManager {
    backend: Arc<dyn Backend>,
    probe: Arc<dyn HardwareProbe>,
}

impl VibrationManager {
    /// Build the manager from a session's backend + probe seam.
    pub fn new(backend: Arc<dyn Backend>, probe: Arc<dyn HardwareProbe>) -> VibrationManager {
        VibrationManager { backend, probe }
    }

    /// Does the descriptor advertise a real motor (and the live probe not demote it)?
    pub fn has_motor(&self) -> bool {
        reconcile_presence(self.backend.is_present("rumble"), &*self.probe, "rumble")
    }

    /// Pulse for `ms` ms. ALWAYS returns the typed no-op shape (`Fired` / `NoopAbsent` /
    /// `NoopSuppressed`) — never fails. The E4 `hapticsEnabled` preference is enforced at this
    /// primitive by the backend.
    pub fn pulse(&self, ms: u32) -> RumbleStatus {
        self.backend.rumble_pulse(ms)
    }
}
