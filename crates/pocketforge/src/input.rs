//! Action/intent mapping over physical buttons (briefing §A "single highest-value input idea";
//! R-G: an E2 API deliverable, but **not** v1-consumed — designed, not over-invested).
//!
//! Apps target **named actions** (`confirm`, `cancel`, …); the descriptor's positional input
//! ids (`south`/`east`/`home`/`l3`/…) + `accept_default` bind them, so the a133→a523 delta
//! (the extra `home`/`l3`/`r3` rows) is invisible to the app. This module is the descriptor-
//! derived *map*; the input HOT PATH (reading events) is the shared `uinput` fd from SPIKE-1 /
//! child `.6`, NOT this RPC-free API.

use crate::descriptor::Descriptor;

/// One bindable physical control, lifted from a descriptor `[[inputs]]` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputAction {
    /// Positional id (`south`, `east`, `home`, `l3`, `dpad`, `lstick`, `ltrig`, …).
    pub id: String,
    /// `button` / `hat` / `stick` / `stick-click` / `trigger`.
    pub kind: String,
    /// Canonical Linux input-event code(s), e.g. `["BTN_A"]` or `["ABS_X", "ABS_Y"]`.
    pub codes: Vec<String>,
    /// The printed glyph, if any (`"A"`, `"B"`, …).
    pub label: Option<String>,
}

/// The descriptor-derived action map handed out by `acquire::<Input>()`.
#[derive(Debug, Clone)]
pub struct InputMap {
    actions: Vec<InputAction>,
    accept_default: Option<String>,
}

impl InputMap {
    /// Build the map from a descriptor (zero per-device code: a133/a523 differ only by rows).
    pub fn from_descriptor(d: &Descriptor) -> InputMap {
        let actions = d
            .inputs
            .iter()
            .map(|i| InputAction {
                id: i.id.clone(),
                kind: i.kind.clone(),
                codes: i.code.split(',').map(|s| s.trim().to_string()).collect(),
                label: i.label.clone(),
            })
            .collect();
        InputMap { actions, accept_default: d.accept_default.clone() }
    }

    /// Every bindable control on this device.
    pub fn actions(&self) -> &[InputAction] {
        &self.actions
    }

    /// Look up a control by positional id.
    pub fn by_id(&self, id: &str) -> Option<&InputAction> {
        self.actions.iter().find(|a| a.id == id)
    }

    /// Resolve a named intent to a physical control id. v0 understands the universal
    /// confirm/cancel pair (driven by `accept_default`); richer maps are forward-looking (R-G).
    pub fn resolve(&self, intent: &str) -> Option<&str> {
        let confirm = self.accept_default.as_deref().unwrap_or("south");
        // The cancel position is the face button "next to" confirm on a standard 4-face pad:
        // south↔east are the confirm/cancel pair in both Xbox- and Switch-region layouts.
        let cancel = if confirm == "south" { "east" } else { "south" };
        let target = match intent {
            "confirm" | "accept" | "ok" => confirm,
            "cancel" | "back" => cancel,
            other => other, // pass-through: an app may target a positional id directly
        };
        self.by_id(target).map(|a| a.id.as_str())
    }
}
