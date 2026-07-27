//! The **prompt plan** — the ordered list of controls the engine walks, one prompt each.
//!
//! A plan is pure INTENT: it says "ask the user to press SOUTH, then EAST, ... then sweep the
//! LEFT STICK, then squeeze the LEFT TRIGGER". It carries the control's positional `id`, its
//! `kind`, and human prompt text. It carries NO `ev_type`/`code` — those are what the engine
//! DISCOVERS by observing the raw events each prompt produces. That separation is the whole
//! point of guided collection: a brand-new device has no descriptor, so the codes cannot be
//! known ahead of time; only the *shape* of a gamepad (which controls to ask for) is known.

/// The kind of a control — mirrors the schema's `input.kind` enum, plus `HatDir` (an internal
/// PROMPT-only kind, never emitted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Button,
    Hat,
    Stick,
    StickClick,
    Trigger,
    /// One D-PAD DIRECTION (up/down/left/right) captured as its own atomic step — a single hat-axis
    /// actuation. The owner presses one direction and the prompt advances (tsp-bwrg.6: the lumped
    /// "press all four" hat step could never satisfy its own two-axis completion on a human's
    /// sequential presses). This kind is PROMPT-only: the four dpad direction captures are MERGED at
    /// emit into the single `hat` row (`ABS_HAT0X,ABS_HAT0Y`), so the collected map is unchanged and
    /// `hat` is never a schema `input.kind` value emitted from a HatDir.
    HatDir,
}

impl Kind {
    /// The schema spelling (`input.kind`). `HatDir` reports `hat` for completeness but is never
    /// emitted directly (its captures merge into one `hat` row at emit).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Button => "button",
            Kind::Hat => "hat",
            Kind::Stick => "stick",
            Kind::StickClick => "stick-click",
            Kind::Trigger => "trigger",
            Kind::HatDir => "hat",
        }
    }

    /// How many distinct axis codes this kind expects to record. Buttons/clicks/triggers record one
    /// `EV_KEY`/axis; a hat and a stick record two `EV_ABS` axes; a single dpad DIRECTION (`HatDir`)
    /// records ONE axis atomically (the four directions merge to the two hat axes at emit).
    pub fn expected_axes(self) -> usize {
        match self {
            Kind::Button | Kind::StickClick | Kind::Trigger | Kind::HatDir => 1,
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
    /// The evdev NODE this control lives on, by name — the descriptor's `input.source` field
    /// (tsp-bwrg.16). `None` = the PRIMARY gamepad node (the run's default source). A device with
    /// more than one input node — the a133 has THREE (the `TRIMUI Player1` gamepad, the
    /// `sunxi-keyboard` LRADC where VOL± live, the `audiocodec` Audio Jack) — names the non-default
    /// node here so the engine reads that control from the right place. The engine routes to it via
    /// [`crate::source::EventSource::set_active_source`] before pumping each control; a single-node
    /// source ignores the hint (default no-op), so single-source runs are unchanged.
    pub source: Option<String>,
}

impl ControlSpec {
    pub fn new(id: &str, kind: Kind, prompt: &str, optional: bool) -> ControlSpec {
        ControlSpec { id: id.to_string(), kind, prompt: prompt.to_string(), optional, source: None }
    }

    /// Builder: pin this control to a NON-primary evdev node (its descriptor `source`).
    pub fn on_source(mut self, node: &str) -> ControlSpec {
        self.source = Some(node.to_string());
        self
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

/// The **A133 (TrimUI Smart Pro) GAMEPAD-NODE** prompt plan — the controls on the `TRIMUI Player1`
/// gamepad node (event2), sourced from the **tsp-ozbp.9 frozen parity baseline** (owner-verified,
/// actuated on real silicon 2026-07-26). That measured ground truth OUTRANKS what the descriptor
/// used to claim; the descriptor is now reconciled to silicon (tsp-ozbp.13: sticks are unsigned
/// 12-bit `0..4095`, not signed16; L2/R2 are the endpoint-only binary `ABS_Z`/`ABS_RZ` axes carried
/// as `semantics = "binary"`, not `BTN_TL2`/`BTN_TR2`).
///
/// ⚠ FRAME (tsp-bwrg.16 — do NOT re-broaden this into a device-scoped claim). These 17 prompts are
/// **exactly the controls on the GAMEPAD node, and nothing more on THAT node** — the owner actuated
/// all of its 17 evdev codes (11 buttons + 6 axes) and it advertises no others: no guide/home beyond
/// MENU (`BTN_MODE`, SDL `guide`) and no `l3`/`r3` clicks (no `BTN_THUMBL/THUMBR`). It is **NOT** the
/// whole device: the a133 exposes **three** input nodes, and this plan reads only the first —
///   • event2 `TRIMUI Player1` — this gamepad node (the 17 below);
///   • event0 `sunxi-keyboard` — the LRADC node carrying the SYSTEM keys **VOL+ / VOL-**
///     (`KEY_VOLUMEUP`/`KEY_VOLUMEDOWN`), which this single-node plan structurally cannot see
///     (they are absent from the gamepad node's KEY bitmap — tsp-ozbp.2 / tsp-bwrg.16);
///   • event1 `audiocodec sunxi Audio Jack` — the wired-headset inline remote (also 114/115 +
///     `KEY_MEDIA`/`KEY_VOICECOMMAND`).
/// The earlier phrasing here — "the device has EXACTLY these controls and NOTHING ELSE" — was the
/// exact node-scoped-observation-written-as-a-device-fact defect this bead exists to kill: it read
/// one node and declared the whole device, which is how "we missed the volume buttons" happened.
/// Capturing the event0/event1 controls needs the descriptor-driven MULTI-SOURCE collection
/// (`ControlSpec::source` + `EventSource::set_active_source`); wiring VOL± into an a133 plan is a
/// follow-up coupled to the owner-attended live-confirm pass (deferred), not done here.
///
/// Every gamepad-node control here is **required** — none is `optional`, nothing flash-then-skips,
/// no phantom is prompted. "Prompt for what the device HAS, full stop" (owner directive,
/// 2026-07-27). The descriptor's gamepad rows agree with this SAME baseline, so the two agreeing is
/// checkable, not coincidental.
pub fn a133_gamepad_plan() -> Vec<ControlSpec> {
    vec![
        // Prompt text here is POSITION only and never carries an A/B/X/Y letter. The map is by SDL
        // position (south=bottom -> BTN_A, etc.); the PRINTED faceplate glyph differs by chassis (a
        // TrimUI is Nintendo-arranged: bottom is "B", not "A"), so the wizard appends the device's
        // real glyph from the descriptor `label` field at render time — never a letter derived here
        // (tsp-bwrg.6 owner pass #5: "...BOTTOM FACE BUTTON (A)" pointed at a button printed "B").
        ControlSpec::new("south", Kind::Button, "Press the BOTTOM face button", false),
        ControlSpec::new("east", Kind::Button, "Press the RIGHT face button", false),
        ControlSpec::new("west", Kind::Button, "Press the LEFT face button", false),
        ControlSpec::new("north", Kind::Button, "Press the TOP face button", false),
        ControlSpec::new("select", Kind::Button, "Press SELECT", false),
        ControlSpec::new("start", Kind::Button, "Press START", false),
        // Menu button — evdev BTN_MODE, which SDL names `guide` (caps.py 0x13c -> guide). The
        // device HAS this control; the old generic plan's "GUIDE / MENU (skip if none)" optional
        // entry is what the owner read as a phantom HOME button. Prompt it plainly as MENU.
        ControlSpec::new("guide", Kind::Button, "Press the MENU button", false),
        ControlSpec::new("l1", Kind::Button, "Press the LEFT shoulder (L1)", false),
        ControlSpec::new("r1", Kind::Button, "Press the RIGHT shoulder (R1)", false),
        // The D-PAD as FOUR atomic per-direction steps (owner-directed, tsp-bwrg.6). Each is its own
        // saw-it→advance capture (a single hat-axis actuation), so the fragile lumped "press all
        // four" two-axis completion — which a human's sequential presses could never satisfy — is
        // gone entirely. The four captures MERGE at emit into the single hat row
        // (`ABS_HAT0X,ABS_HAT0Y`), so the collected map is UNCHANGED — only the prompt UX splits.
        // With these four, the plan is exactly the 17 prompts of the tsp-ozbp.9 frozen baseline 1:1.
        ControlSpec::new("dpad_up", Kind::HatDir, "Press UP on the D-PAD", false),
        ControlSpec::new("dpad_down", Kind::HatDir, "Press DOWN on the D-PAD", false),
        ControlSpec::new("dpad_left", Kind::HatDir, "Press LEFT on the D-PAD", false),
        ControlSpec::new("dpad_right", Kind::HatDir, "Press RIGHT on the D-PAD", false),
        // A stick completes on ONE full circle that touches all four edges (both axes reach both
        // extremes). Prompt for exactly that ONE sweep — never "both directions"/back-and-forth,
        // which reads as a second roll and confused the owner when the step advanced after one
        // (tsp-bwrg.6 pass #5: "it stopped after 1 roll ... just want to be consistent").
        ControlSpec::new(
            "lstick",
            Kind::Stick,
            "Roll the LEFT STICK once all the way around the circle (touch every edge)",
            false,
        ),
        ControlSpec::new(
            "rstick",
            Kind::Stick,
            "Roll the RIGHT STICK once all the way around the circle (touch every edge)",
            false,
        ),
        // L2/R2 are BINARY on the a133 — endpoint-only ABS_Z/ABS_RZ (semantics="binary" in the
        // descriptor), a press with no proportional travel, not an analog squeeze. Prompt as a press.
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
    fn a133_plan_is_the_17_prompt_frozen_baseline_one_to_one() {
        let plan = a133_gamepad_plan();
        let ids: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();
        // 17 PROMPTS, 1:1 with the tsp-ozbp.9 frozen parity baseline: 9 buttons + 4 dpad DIRECTIONS
        // + 2 sticks + 2 triggers. The dpad is four atomic direction prompts that MERGE to one hat
        // row at emit (see collect::Collector::emit), so the prompt plan and the measured ground
        // truth are now the same 17-item list (no lumped/collapsed entry).
        assert_eq!(
            ids,
            [
                "south", "east", "west", "north", "select", "start", "guide", "l1", "r1",
                "dpad_up", "dpad_down", "dpad_left", "dpad_right",
                "lstick", "rstick", "ltrig", "rtrig",
            ],
            "a133 plan must be exactly the 17-prompt frozen baseline"
        );
        assert_eq!(plan.len(), 17, "the a133 plan is 17 prompts (baseline 1:1)");
        // No phantom, and NO lumped single "dpad" entry.
        for phantom in ["l3", "r3", "home", "capture", "misc", "dpad"] {
            assert!(!ids.contains(&phantom), "a133 plan must not prompt {phantom}");
        }
        // EVERY prompt is required (no flash-then-skip); each dpad direction is an atomic HatDir.
        for c in &plan {
            assert!(!c.optional, "a133 control {} must be required (no flash-then-skip)", c.id);
        }
        for d in ["dpad_up", "dpad_down", "dpad_left", "dpad_right"] {
            let c = plan.iter().find(|c| c.id == d).unwrap();
            assert_eq!(c.kind, Kind::HatDir, "{d} must be an atomic HatDir step");
        }
    }

    /// FRAME PIN (tsp-bwrg.16, AC#4). The a133 plan reads exactly ONE node — the primary gamepad
    /// node — so every control it carries is on it (`source == None`), and NO system/volume control
    /// has leaked in (those live on the sunxi-keyboard / audiocodec nodes). This is the guard the
    /// old "the device has EXACTLY these controls and NOTHING ELSE" comment lacked: it FAILS if a
    /// future edit re-broadens this gamepad-NODE plan into a device-wide claim — either by smuggling
    /// a system key into it or by pinning a control to a non-primary `source` without going through
    /// the deliberate multi-source path. A node-scoped plan must stay node-scoped.
    #[test]
    fn a133_plan_is_gamepad_node_scoped_only() {
        let plan = a133_gamepad_plan();
        for c in &plan {
            assert_eq!(
                c.source, None,
                "control '{}' is pinned to a non-primary source — the a133 GAMEPAD-node plan is \
                 single-node by construction; a multi-node control belongs to the multi-source \
                 path (ControlSpec::on_source + a MultiSource run), not smuggled in here",
                c.id
            );
        }
        for sys in ["vol_up", "vol_down", "volumeup", "volumedown", "home", "power", "mute"] {
            assert!(
                !plan.iter().any(|c| c.id == sys),
                "system control '{sys}' must NOT appear in the a133 gamepad-node plan — it lives on \
                 a DIFFERENT evdev node (sunxi-keyboard / audiocodec), and claiming it here would \
                 repeat the node-scoped-observation-as-device-fact defect this bead fixed"
            );
        }
    }
}
