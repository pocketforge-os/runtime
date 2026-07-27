//! The **prompt plan** — the ordered list of controls the engine walks, one prompt each.
//!
//! A plan is pure INTENT: it says "ask the user to press SOUTH, then EAST, ... then sweep the
//! LEFT STICK, then squeeze the LEFT TRIGGER". It carries the control's positional `id`, its
//! `kind`, and human prompt text. It carries NO `ev_type`/`code` — those are what the engine
//! DISCOVERS by observing the raw events each prompt produces. That separation is the whole
//! point of guided collection: a brand-new device has no descriptor, so the codes cannot be
//! known ahead of time; only the *shape* of a gamepad (which controls to ask for) is known.

/// The kind of a control — mirrors the schema's `input.kind` enum exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Button,
    Hat,
    Stick,
    StickClick,
    Trigger,
}

impl Kind {
    /// The schema spelling (`input.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Button => "button",
            Kind::Hat => "hat",
            Kind::Stick => "stick",
            Kind::StickClick => "stick-click",
            Kind::Trigger => "trigger",
        }
    }

    /// How many distinct axis codes this kind expects to record (buttons/clicks record one
    /// `EV_KEY` code; a hat and a stick record two `EV_ABS` axes; a trigger records one).
    pub fn expected_axes(self) -> usize {
        match self {
            Kind::Button | Kind::StickClick | Kind::Trigger => 1,
            Kind::Hat | Kind::Stick => 2,
        }
    }
}

/// One prompt in the plan.
#[derive(Clone, Debug)]
pub struct ControlSpec {
    /// Positional id — schema pattern `^[a-z0-9_]+$` (e.g. `south`, `dpad`, `lstick`, `ltrig`).
    pub id: String,
    pub kind: Kind,
    /// Human prompt text shown/spoken for this control.
    pub prompt: String,
    /// If `true`, a control that never fires is SKIPPED (row omitted), not an error — for
    /// controls a given chassis may not physically wire (e.g. `guide`, stick-clicks). A NEW
    /// device's guided run leaves these optional so "missing hardware = row omission".
    pub optional: bool,
}

impl ControlSpec {
    fn new(id: &str, kind: Kind, prompt: &str, optional: bool) -> ControlSpec {
        ControlSpec { id: id.to_string(), kind, prompt: prompt.to_string(), optional }
    }
}

/// The default generic-gamepad plan (the a133 base chassis shape + the common optional extras).
///
/// Required controls are the base A133's physical set; `guide`, the stick-clicks, and the
/// hat-vs-nothing are `optional` so a run against a chassis that lacks them omits the row rather
/// than failing. Order is press-friendly: faces, system, shoulders, dpad, sticks, triggers.
pub fn default_gamepad_plan() -> Vec<ControlSpec> {
    vec![
        ControlSpec::new("south", Kind::Button, "Press the BOTTOM face button (south)", false),
        ControlSpec::new("east", Kind::Button, "Press the RIGHT face button (east)", false),
        ControlSpec::new("west", Kind::Button, "Press the LEFT face button (west)", false),
        ControlSpec::new("north", Kind::Button, "Press the TOP face button (north)", false),
        ControlSpec::new("select", Kind::Button, "Press SELECT", false),
        ControlSpec::new("start", Kind::Button, "Press START", false),
        ControlSpec::new("guide", Kind::Button, "Press GUIDE / MENU (skip if none)", true),
        ControlSpec::new("l1", Kind::Button, "Press the LEFT shoulder (L1)", false),
        ControlSpec::new("r1", Kind::Button, "Press the RIGHT shoulder (R1)", false),
        ControlSpec::new(
            "dpad",
            Kind::Hat,
            "Press each D-PAD direction: UP, DOWN, LEFT, RIGHT",
            true,
        ),
        ControlSpec::new(
            "lstick",
            Kind::Stick,
            "Sweep the LEFT STICK fully in a circle (all the way in every direction)",
            false,
        ),
        ControlSpec::new(
            "rstick",
            Kind::Stick,
            "Sweep the RIGHT STICK fully in a circle (all the way in every direction)",
            false,
        ),
        ControlSpec::new("l3", Kind::StickClick, "Click the LEFT STICK (L3) (skip if none)", true),
        ControlSpec::new("r3", Kind::StickClick, "Click the RIGHT STICK (R3) (skip if none)", true),
        ControlSpec::new(
            "ltrig",
            Kind::Trigger,
            "Squeeze the LEFT TRIGGER (L2) slowly from released to fully pressed",
            false,
        ),
        ControlSpec::new(
            "rtrig",
            Kind::Trigger,
            "Squeeze the RIGHT TRIGGER (R2) slowly from released to fully pressed",
            false,
        ),
    ]
}

/// The **A133 (TrimUI Smart Pro)** prompt plan — the controls THIS device actually has, sourced
/// from the **tsp-ozbp.9 frozen parity baseline** (owner-verified, actuated on real silicon
/// 2026-07-26). That measured ground truth OUTRANKS the known-wrong `a133/capabilities.toml`
/// descriptor (tsp-ozbp.13: it lists analog `ABS_Z`/`ABS_RZ` triggers when L2/R2 are binary
/// `BTN_TL2`/`BTN_TR2`, and signed16 stick ranges when the real axes are unsigned 0..4095).
///
/// The device has EXACTLY these 14 controls and NOTHING ELSE — the owner actuated all 17 evdev
/// codes (11 buttons + 6 axes) and no others exist: there is **no** guide/home button beyond MENU
/// (`BTN_MODE`, which SDL names `guide`) and **no** `l3`/`r3` stick clicks (no `BTN_THUMBL/THUMBR`).
/// So every control here is **required** — none is `optional`, nothing flash-then-skips, and no
/// phantom control is ever prompted. This is not an app-side filter of a generic list; it is the
/// device's real control set. "Prompt for what the device HAS, full stop" (owner directive,
/// 2026-07-27). tsp-ozbp.13 must be corrected to match this SAME baseline (a different lane); the
/// two agreeing is then checkable, not coincidental.
pub fn a133_gamepad_plan() -> Vec<ControlSpec> {
    vec![
        ControlSpec::new("south", Kind::Button, "Press the BOTTOM face button (A)", false),
        ControlSpec::new("east", Kind::Button, "Press the RIGHT face button (B)", false),
        ControlSpec::new("west", Kind::Button, "Press the LEFT face button (X)", false),
        ControlSpec::new("north", Kind::Button, "Press the TOP face button (Y)", false),
        ControlSpec::new("select", Kind::Button, "Press SELECT", false),
        ControlSpec::new("start", Kind::Button, "Press START", false),
        // Menu button — evdev BTN_MODE, which SDL names `guide` (caps.py 0x13c -> guide). The
        // device HAS this control; the old generic plan's "GUIDE / MENU (skip if none)" optional
        // entry is what the owner read as a phantom HOME button. Prompt it plainly as MENU.
        ControlSpec::new("guide", Kind::Button, "Press the MENU button", false),
        ControlSpec::new("l1", Kind::Button, "Press the LEFT shoulder (L1)", false),
        ControlSpec::new("r1", Kind::Button, "Press the RIGHT shoulder (R1)", false),
        // A hat needs BOTH axes (HAT0X and HAT0Y), so ask for all four directions.
        ControlSpec::new(
            "dpad",
            Kind::Hat,
            "Press ALL FOUR D-PAD directions: UP, DOWN, LEFT, RIGHT",
            false,
        ),
        // A stick needs BOTH axes actuated — a partial arc only moves one, so ask for a full circle.
        ControlSpec::new(
            "lstick",
            Kind::Stick,
            "Roll the LEFT STICK all the way around in a FULL CIRCLE (both directions)",
            false,
        ),
        ControlSpec::new(
            "rstick",
            Kind::Stick,
            "Roll the RIGHT STICK all the way around in a FULL CIRCLE (both directions)",
            false,
        ),
        // L2/R2 are BINARY buttons on the a133 (BTN_TL2/BTN_TR2) — a press, not an analog squeeze.
        ControlSpec::new("ltrig", Kind::Trigger, "Press the LEFT TRIGGER (L2) fully", false),
        ControlSpec::new("rtrig", Kind::Trigger, "Press the RIGHT TRIGGER (R2) fully", false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_and_axis_counts_match_schema() {
        assert_eq!(Kind::Button.as_str(), "button");
        assert_eq!(Kind::Hat.as_str(), "hat");
        assert_eq!(Kind::Stick.as_str(), "stick");
        assert_eq!(Kind::StickClick.as_str(), "stick-click");
        assert_eq!(Kind::Trigger.as_str(), "trigger");
        assert_eq!(Kind::Button.expected_axes(), 1);
        assert_eq!(Kind::Hat.expected_axes(), 2);
        assert_eq!(Kind::Stick.expected_axes(), 2);
        assert_eq!(Kind::Trigger.expected_axes(), 1);
    }

    #[test]
    fn default_plan_covers_the_a133_base_controls() {
        let ids: Vec<_> = default_gamepad_plan().into_iter().map(|c| c.id).collect();
        for want in ["south", "east", "west", "north", "select", "start", "l1", "r1", "dpad",
            "lstick", "rstick", "ltrig", "rtrig"]
        {
            assert!(ids.contains(&want.to_string()), "plan missing {want}");
        }
    }

    #[test]
    fn a133_plan_is_exactly_the_frozen_baseline_controls_all_required() {
        let plan = a133_gamepad_plan();
        let ids: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();
        // EXACTLY the tsp-ozbp.9 frozen-baseline controls, in press-friendly order — no more, no less.
        assert_eq!(
            ids,
            [
                "south", "east", "west", "north", "select", "start", "guide", "l1", "r1", "dpad",
                "lstick", "rstick", "ltrig", "rtrig",
            ],
            "a133 plan must be exactly the frozen-baseline control set"
        );
        // The phantom controls the owner tripped on must NOT appear (no home/guide-optional, no L3/R3).
        for phantom in ["l3", "r3", "home", "capture", "misc"] {
            assert!(!ids.contains(&phantom), "a133 plan must not prompt phantom control {phantom}");
        }
        // EVERY control is required — nothing is optional, so nothing can flash-then-skip.
        for c in &plan {
            assert!(!c.optional, "a133 control {} must be required (no flash-then-skip)", c.id);
        }
    }
}
