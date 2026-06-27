//! The **input manager** — the facade-side action map over evdev. Apps target NAMED actions
//! (`confirm`/`cancel`/…) and bind positional controls from the descriptor, so the a133→a523
//! delta (the extra `home`/`l3`/`r3` rows) is invisible with ZERO per-device code — only the
//! descriptor data differs. This manager is the descriptor-derived map (the RPC-free API);
//! the per-event HOT PATH (the shared `uinput`+`EVIOCGRAB` fd from SPIKE-1) is child `.6`, not
//! this layer.

use std::sync::Arc;

use crate::backend::Backend;
use crate::descriptor::Descriptor;
use crate::input::{InputAction, InputMap};

use super::{reconcile_presence, HardwareProbe};

/// One device-agnostic input object: the action map plus presence reconciliation.
pub struct InputManager {
    backend: Arc<dyn Backend>,
    probe: Arc<dyn HardwareProbe>,
    map: InputMap,
}

impl InputManager {
    /// Build the manager — the action map is derived from the descriptor (zero per-device code).
    pub fn new(
        descriptor: &Descriptor,
        backend: Arc<dyn Backend>,
        probe: Arc<dyn HardwareProbe>,
    ) -> InputManager {
        let map = InputMap::from_descriptor(descriptor);
        InputManager { backend, probe, map }
    }

    /// Is input present? (Every device has controls, but the probe seam is honored for symmetry.)
    pub fn present(&self) -> bool {
        reconcile_presence(self.backend.is_present("input"), &*self.probe, "input")
    }

    /// The descriptor-derived action map (named-action resolution + all bindable controls).
    pub fn map(&self) -> &InputMap {
        &self.map
    }

    /// Resolve a named intent (`confirm`/`cancel`/…) to a physical control id.
    pub fn resolve(&self, intent: &str) -> Option<&str> {
        self.map.resolve(intent)
    }

    /// Every bindable control on this device.
    pub fn actions(&self) -> &[InputAction] {
        self.map.actions()
    }

    /// Look up a control by positional id.
    pub fn by_id(&self, id: &str) -> Option<&InputAction> {
        self.map.by_id(id)
    }
}
