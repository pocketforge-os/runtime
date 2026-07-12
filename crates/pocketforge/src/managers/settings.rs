//! The **settings manager** — the device-agnostic **accessibility preference surface (E4)**.
//! Preferences (`hapticsEnabled`, `reduceMotion`, `monoAudio`, `brightness`) route through the
//! backend's persistent preference store, which the per-capability primitives read AT the point of
//! actuation (the E4 enforcement point — see [`super::vibration`]). On the in-process backend a
//! set is persisted + observed immediately; over the broker, preference ops await E4's post-Phase-2
//! wire op, so the v0 broker client returns the caller's default and cannot observe — named, not
//! hidden ([`Backend::subscribe_preference`] returns `None` there).
//!
//! ## Read-only to apps (R-A, owner ruling Q4)
//!
//! The app-facing surface is **read + observe**: [`get_bool`](SettingsManager::get_bool) /
//! [`get_scalar`](SettingsManager::get_scalar) / the named typed readers, plus
//! [`subscribe`](SettingsManager::subscribe) for live `PrefsDidChange`. Writes are NOT part of the
//! app contract — they ride the authority-side write path (`pf-settings` CLI, the `.3` UI, the
//! supervisor). [`set_bool`](SettingsManager::set_bool) exists for the in-process CONTROL PLANE
//! (tests + the sim's injection-as-API surface), not for app code; see `docs/PREFERENCES.md`.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::backend::{Backend, PrefValue};

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

    /// Read an accessibility/user preference scalar (the E4 seam; `brightness` today), or
    /// `default` if the backend has no store-backed value.
    pub fn get_scalar(&self, name: &str, default: i64) -> i64 {
        self.backend.preference_scalar(name, default)
    }

    // --- Named typed readers for the v1 schema (the app-facing read-API) -----------------------
    // Defaults mirror the pf-prefs schema so a store-less/broker backend reads the same values.

    /// `reduceMotion` — suppress non-essential cosmetic motion. **Readable + observable seam
    /// only**: v0 ships NO cosmetic-motion machinery, so nothing is auto-suppressed — an app (or a
    /// future broker-driven animator) reads this and honors it cooperatively. See `docs/PREFERENCES.md`.
    pub fn reduce_motion(&self) -> bool {
        self.get_bool("reduceMotion", false)
    }

    /// `hapticsEnabled` — honored AT the primitive by the vibration path (off ⇒ a silent no-op via
    /// the SAME shape as an absent motor). Exposed here as a read for apps that want to reflect it.
    pub fn haptics_enabled(&self) -> bool {
        self.get_bool("hapticsEnabled", true)
    }

    /// `monoAudio` — down-mix to mono; honored on the audio routing layer (see [`super::audio`]).
    pub fn mono_audio(&self) -> bool {
        self.get_bool("monoAudio", false)
    }

    /// `brightness` — 0..=100. **CONTRACT-ONLY in v1** (owner ruling Q3): readable + observable,
    /// with NO sysfs apply leg anywhere in this epic (a133 has no `/sys/class/backlight`; the
    /// per-SoC hardware leg is a hardware-gated follow-on bead).
    pub fn brightness(&self) -> i64 {
        self.get_scalar("brightness", 100)
    }

    /// Set an accessibility/user preference bool via the in-process CONTROL PLANE (tests + the
    /// sim's injection-as-API surface — NOT the app contract, which is read-only). The
    /// per-capability primitive reads this at actuation time, so e.g. `set_bool("hapticsEnabled",
    /// false)` makes vibration a no-op via the SAME path as an absent motor (the E4 unification),
    /// persisting through the store and firing [`subscribe`](Self::subscribe) observers.
    pub fn set_bool(&self, name: &str, value: bool) {
        self.backend.set_preference_bool(name, value)
    }

    /// Observe live `PrefsDidChange` for a preference `name` (the E4 observer). Returns
    /// `Some(Receiver)` yielding the new effective [`PrefValue`] on every write path (control
    /// plane, `pf-settings` CLI via reload, future UI) on a backend that can observe in-process
    /// (the v0 in-process backend); `None` on one that cannot (the broker client — no preference
    /// wire op in v0). This is the "a running app reacts the instant a preference flips" surface.
    pub fn subscribe(&self, name: &str) -> Option<Receiver<PrefValue>> {
        self.backend.subscribe_preference(name)
    }
}
