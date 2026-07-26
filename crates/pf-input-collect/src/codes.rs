//! Reverse `code -> name` tables for the evdev event codes the descriptor vocabulary uses.
//!
//! Ported verbatim (same restricted vocab) from
//! `platform/regression/caps/evdev-probe.py` — whose `BTN`/`KEY`/`ABS` dicts are themselves
//! GENERATED from the kernel ABI (`input-event-codes.h`) restricted to exactly the
//! `core/caps.py` schema vocab. Keeping the two in lockstep is deliberate: the collection
//! engine must name a code the SAME way the ground-truth dumper does, or the candidate it
//! emits would not diff cleanly against a probe capture. Do not hand-extend with codes the
//! schema's `code` pattern (`^(BTN_|KEY_|ABS_)...`) cannot express.

/// `EV_SYN` — report-boundary event type.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY` — key/button event type.
pub const EV_KEY: u16 = 0x01;
/// `EV_ABS` — absolute-axis event type.
pub const EV_ABS: u16 = 0x03;
/// `SYN_REPORT` — commit the current event report.
pub const SYN_REPORT: u16 = 0x00;

/// The schema `ev_type` spelling for an event type (only `EV_KEY`/`EV_ABS` are descriptor-valid).
pub fn ev_type_name(ev_type: u16) -> Option<&'static str> {
    match ev_type {
        EV_KEY => Some("EV_KEY"),
        EV_ABS => Some("EV_ABS"),
        _ => None,
    }
}

/// `EV_KEY` code -> canonical `BTN_*`/`KEY_*` name (schema vocab only).
pub fn key_name(code: u16) -> Option<&'static str> {
    Some(match code {
        // --- BTN_* (joystick/gamepad) ---
        0x120 => "BTN_TRIGGER",
        0x121 => "BTN_THUMB",
        0x130 => "BTN_A",
        0x131 => "BTN_B",
        0x132 => "BTN_C",
        0x133 => "BTN_X",
        0x134 => "BTN_Y",
        0x135 => "BTN_Z",
        0x136 => "BTN_TL",
        0x137 => "BTN_TR",
        0x138 => "BTN_TL2",
        0x139 => "BTN_TR2",
        0x13a => "BTN_SELECT",
        0x13b => "BTN_START",
        0x13c => "BTN_MODE",
        0x13d => "BTN_THUMBL",
        0x13e => "BTN_THUMBR",
        0x220 => "BTN_DPAD_UP",
        0x221 => "BTN_DPAD_DOWN",
        0x222 => "BTN_DPAD_LEFT",
        0x223 => "BTN_DPAD_RIGHT",
        // --- KEY_* (system keys some pads route to input) ---
        0x1 => "KEY_ESC",
        0x1c => "KEY_ENTER",
        0x66 => "KEY_HOME",
        0x72 => "KEY_VOLUMEDOWN",
        0x73 => "KEY_VOLUMEUP",
        0x74 => "KEY_POWER",
        0x8b => "KEY_MENU",
        0x9e => "KEY_BACK",
        0xac => "KEY_HOMEPAGE",
        _ => return None,
    })
}

/// `EV_ABS` code -> canonical `ABS_*` name (schema vocab only).
pub fn abs_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x0 => "ABS_X",
        0x1 => "ABS_Y",
        0x2 => "ABS_Z",
        0x3 => "ABS_RX",
        0x4 => "ABS_RY",
        0x5 => "ABS_RZ",
        0x6 => "ABS_THROTTLE",
        0x7 => "ABS_RUDDER",
        0x9 => "ABS_GAS",
        0xa => "ABS_BRAKE",
        0x10 => "ABS_HAT0X",
        0x11 => "ABS_HAT0Y",
        0x12 => "ABS_HAT1X",
        0x13 => "ABS_HAT1Y",
        _ => return None,
    })
}

/// Name an `(ev_type, code)` pair the descriptor way; `None` if either is outside schema vocab.
pub fn code_name(ev_type: u16, code: u16) -> Option<&'static str> {
    match ev_type {
        EV_KEY => key_name(code),
        EV_ABS => abs_name(code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_a133_face_and_axis_codes_name_correctly() {
        assert_eq!(key_name(0x130), Some("BTN_A"));
        assert_eq!(key_name(0x131), Some("BTN_B"));
        assert_eq!(key_name(0x133), Some("BTN_X"));
        assert_eq!(key_name(0x134), Some("BTN_Y"));
        assert_eq!(key_name(0x13a), Some("BTN_SELECT"));
        assert_eq!(key_name(0x13b), Some("BTN_START"));
        assert_eq!(key_name(0x13c), Some("BTN_MODE"));
        assert_eq!(key_name(0x136), Some("BTN_TL"));
        assert_eq!(key_name(0x137), Some("BTN_TR"));
        assert_eq!(abs_name(0x0), Some("ABS_X"));
        assert_eq!(abs_name(0x1), Some("ABS_Y"));
        assert_eq!(abs_name(0x3), Some("ABS_RX"));
        assert_eq!(abs_name(0x4), Some("ABS_RY"));
        assert_eq!(abs_name(0x2), Some("ABS_Z"));
        assert_eq!(abs_name(0x5), Some("ABS_RZ"));
        assert_eq!(abs_name(0x10), Some("ABS_HAT0X"));
        assert_eq!(abs_name(0x11), Some("ABS_HAT0Y"));
    }

    #[test]
    fn unknown_codes_are_none_not_fabricated() {
        assert_eq!(key_name(0xfff), None);
        assert_eq!(abs_name(0xfe), None);
        assert_eq!(ev_type_name(0x04), None); // EV_MSC is not descriptor-valid
    }
}
