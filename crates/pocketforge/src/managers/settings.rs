//! The **settings manager** — the device-agnostic **accessibility preference surface (E4)**.
//! Boolean preferences (e.g. `hapticsEnabled`) route through the backend's preference store,
//! which the per-capability primitives read AT the point of actuation (the E4 enforcement point —
//! see [`super::vibration`]). On the in-process backend a set is observed immediately; over the
//! broker, preference ops await E4's wire op (`.3`/E4), so the v0 broker client returns the
//! caller's default — named, not hidden. (Richer typed/string settings are deferred to E4's
//! settings schema; this manager is the E4 toggle surface the primitives honor today.)

use std::sync::Arc;

use crate::backend::Backend;

/// One device-agnostic settings/preferences object.
pub struct SettingsManager {
    backend: Arc<dyn Backend>,
}

impl SettingsManager {
    /// Build the manager from a session's backend.
    pub fn new(backend: Arc<dyn Backend>) -> SettingsManager {
        SettingsManager { backend }
    }

    /// Read an accessibility/user preference bool (the E4 seam), or `default` if unset.
    pub fn get_bool(&self, name: &str, default: bool) -> bool {
        self.backend.preference_bool(name, default)
    }

    /// Set an accessibility/user preference bool. The per-capability primitive reads this at
    /// actuation time, so e.g. `set_bool("hapticsEnabled", false)` makes vibration a no-op via
    /// the SAME path as an absent motor (the E4 unification).
    pub fn set_bool(&self, name: &str, value: bool) {
        self.backend.set_preference_bool(name, value)
    }
}
