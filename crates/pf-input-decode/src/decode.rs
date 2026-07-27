//! The control map — turn a per-UART [`Frame`] into evdev events. **This is the delicate part:**
//! a wrong-but-plausible bit→code assignment silently mis-maps a control while looking like it
//! works, which is exactly what the `tsp-bwrg` validation gate exists to catch. Every mapping
//! below is pinned to the `tsp-ozbp.2` ground truth and the `platform/devices/a133/
//! capabilities.toml` descriptor, and exercised by [`crate::tests`] + the synthetic integration
//! test against EVERY control.
//!
//! ## Ground truth (`tsp-ozbp.2`, with the face bits re-anchored by `tsp-ozbp.14`)
//!
//! - **`ttyS3` = RIGHT** stick + these `byte2` bits:
//!   `0x01`=R1 `0x02`=R2 `0x04`=**north** `0x08`=**west** `0x10`=**east** `0x20`=**south**
//!   `0x40`=Select `0x80`=Start.
//! - **`ttyS4` = LEFT** stick + these `byte2` bits:
//!   `0x01`=L1 `0x02`=L2 `0x04`=Dup `0x08`=Dleft `0x10`=Dright `0x20`=Ddown `0x40`=unused(Pro-S)
//!   `0x80`=Menu.
//!
//! ⚠ The `tsp-ozbp.2` map wrote the four face bits as bare LETTERS (`0x10`=A, `0x20`=B). A letter
//! is ambiguous between the printed glyph and the canonical position, and on this
//! Nintendo-arranged chassis those two disagree — so that map is **not a usable position source**
//! and the letters are deliberately not reproduced above. The bit↔POSITION rows are re-anchored
//! on the position-prompted, owner-confirmed `tsp-bwrg.6` candidate; the derivation is spelled
//! out in `tests/face_position_frame.rs`, which is also the regression bar.
//!
//! ## Which frame does this emit? — **FRAME C, kernel-canonical positional**
//!
//! See [`crate::codes`] for the full two-frame contract (Frame D = driver-emitted/wire,
//! Frame C = kernel-canonical positional). The table below is the ONE place the physical
//! position→code decision is made, so it is the one place the frame can be got wrong: it MUST be
//! keyed on where the button physically is, never on the letter printed on it. Keying it on the
//! glyph is exactly the shipped `tsp-ozbp.14` defect — and because the kernel aliases
//! `BTN_X == BTN_NORTH` / `BTN_Y == BTN_WEST`, a glyph-keyed table is *coincidentally correct*
//! on west/north here, so it looks half-right and reads as a typo rather than a frame error.
//!
//! L2/R2 are physically binary → BUTTONS (`BTN_TL2`/`BTN_TR2`), matching `pf-input-broker`.
//! The d-pad (LEFT only) is emitted as an `ABS_HAT0X`/`ABS_HAT0Y` hat, per the descriptor's
//! `id="dpad" kind="hat"`. `event0`/LRADC VOL± is NOT on this transport — we never touch it.

use crate::codes;
use crate::frame::Frame;

/// Which UART a frame came from — the frame layout is otherwise identical, but the button-bit
/// meanings and the stick's axis codes differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// `/dev/ttyS3` — the RIGHT stick + the right-hand button cluster.
    Right,
    /// `/dev/ttyS4` — the LEFT stick + the left-hand cluster, d-pad, and Menu.
    Left,
}

/// One evdev event to emit: `(ev_type, code, value)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    /// `EV_KEY` or `EV_ABS`.
    pub ev_type: u16,
    /// The button/axis code.
    pub code: u16,
    /// Key press/release (1/0) or the axis value.
    pub value: i32,
}

impl Ev {
    fn key(code: u16, pressed: bool) -> Ev {
        Ev { ev_type: codes::EV_KEY, code, value: pressed as i32 }
    }
    fn abs(code: u16, value: i32) -> Ev {
        Ev { ev_type: codes::EV_ABS, code, value }
    }
}

// --- per-side button bit → code tables -------------------------------------------------------
// ONLY genuine buttons appear here. The LEFT d-pad bits (0x04/0x08/0x10/0x20) and the unused
// Pro-S bit (0x40) are deliberately ABSENT — the d-pad is handled as a hat below, and iterating
// the table (rather than XOR-ing the raw byte) means those bits can never leak out as buttons.

/// RIGHT UART (`ttyS3`) `byte2` bit → evdev button code.
///
/// The four face rows are keyed on PHYSICAL POSITION (Frame C) — the glyph printed on each is
/// noted only so a human at the bench can find the button. Do not re-key this on the glyph.
const RIGHT_BTN: &[(u8, u16)] = &[
    (0x01, codes::BTN_TR),     // R1
    (0x02, codes::BTN_TR2),    // R2 (binary trigger → button)
    (0x04, codes::BTN_NORTH),  // TOP face button    (printed "X")
    (0x08, codes::BTN_WEST),   // LEFT face button   (printed "Y")
    (0x10, codes::BTN_EAST),   // RIGHT face button  (printed "A")
    (0x20, codes::BTN_SOUTH),  // BOTTOM face button (printed "B")
    (0x40, codes::BTN_SELECT), // Select
    (0x80, codes::BTN_START),  // Start
];

/// LEFT UART (`ttyS4`) `byte2` bit → evdev button code (d-pad + unused bit handled separately).
const LEFT_BTN: &[(u8, u16)] = &[
    (0x01, codes::BTN_TL),   // L1
    (0x02, codes::BTN_TL2),  // L2 (binary trigger → button)
    (0x80, codes::BTN_MODE), // Menu (the descriptor's "guide")
];

// LEFT d-pad bits.
const D_UP: u8 = 0x04;
const D_LEFT: u8 = 0x08;
const D_RIGHT: u8 = 0x10;
const D_DOWN: u8 = 0x20;

/// The full set of button codes this device advertises (both sides), in a stable order.
pub fn all_button_codes() -> Vec<u16> {
    RIGHT_BTN.iter().chain(LEFT_BTN.iter()).map(|&(_, c)| c).collect()
}

/// The full set of `(abs_code, min, max)` this device advertises. Sticks are the honest raw
/// 12-bit range; the two hats are `-1..=1`.
pub fn all_axes() -> Vec<(u16, i32, i32)> {
    vec![
        (codes::ABS_X, codes::STICK_MIN, codes::STICK_MAX),
        (codes::ABS_Y, codes::STICK_MIN, codes::STICK_MAX),
        (codes::ABS_RX, codes::STICK_MIN, codes::STICK_MAX),
        (codes::ABS_RY, codes::STICK_MIN, codes::STICK_MAX),
        (codes::ABS_HAT0X, -1, 1),
        (codes::ABS_HAT0Y, -1, 1),
    ]
}

/// Stateful decoder for ONE UART. Holds the last-emitted state so [`apply`] returns only the
/// events that CHANGED (edge-triggered) — the standard evdev contract, and it keeps the report
/// stream lean at 48 fps. The first frame diffs against an all-released / centre-unknown baseline,
/// so a control already held at startup registers, and both stick axes are emitted once so a
/// consumer never reads the uinput default (`0`) as a real rest value.
///
/// [`apply`]: SideDecoder::apply
pub struct SideDecoder {
    side: Side,
    seen: bool,
    last_buttons: u8,
    last_x: u16,
    last_y: u16,
    last_hat_x: i32,
    last_hat_y: i32,
}

impl SideDecoder {
    /// A decoder for `side` with no prior state.
    pub fn new(side: Side) -> SideDecoder {
        SideDecoder {
            side,
            seen: false,
            last_buttons: 0,
            last_x: 0,
            last_y: 0,
            last_hat_x: 0,
            last_hat_y: 0,
        }
    }

    /// The stick's `(x_code, y_code)` for this side.
    fn axis_codes(&self) -> (u16, u16) {
        match self.side {
            Side::Right => (codes::ABS_RX, codes::ABS_RY),
            Side::Left => (codes::ABS_X, codes::ABS_Y),
        }
    }

    fn button_table(&self) -> &'static [(u8, u16)] {
        match self.side {
            Side::Right => RIGHT_BTN,
            Side::Left => LEFT_BTN,
        }
    }

    /// Decode one frame, returning the changed events (empty if nothing moved). The caller emits
    /// them and follows with one `SYN_REPORT` iff the returned slice is non-empty.
    pub fn apply(&mut self, f: Frame) -> Vec<Ev> {
        let mut out = Vec::new();
        let first = !self.seen;

        // Buttons: per-mask edge detection. On the first frame the baseline is all-released, so a
        // held button emits a press and a released one emits nothing.
        for &(mask, code) in self.button_table() {
            let now = f.buttons & mask != 0;
            let was = !first && (self.last_buttons & mask != 0);
            if now != was {
                out.push(Ev::key(code, now));
            }
        }

        // D-pad hat (LEFT UART only). Opposite bits held simultaneously cancel to centre.
        // `last_hat_*` starts at 0 (= centre = the uinput default), so a plain change check also
        // does the right thing on the first frame: a centred d-pad emits nothing (no redundant
        // centre), a held direction emits once.
        if self.side == Side::Left {
            let hat_x = axis_from(f.buttons & D_LEFT != 0, f.buttons & D_RIGHT != 0);
            let hat_y = axis_from(f.buttons & D_UP != 0, f.buttons & D_DOWN != 0);
            if hat_x != self.last_hat_x {
                out.push(Ev::abs(codes::ABS_HAT0X, hat_x));
            }
            if hat_y != self.last_hat_y {
                out.push(Ev::abs(codes::ABS_HAT0Y, hat_y));
            }
            self.last_hat_x = hat_x;
            self.last_hat_y = hat_y;
        }

        // Sticks: emit on change; always emit both axes on the first frame so the real value
        // replaces the uinput default immediately.
        let (xc, yc) = self.axis_codes();
        if first || f.x != self.last_x {
            out.push(Ev::abs(xc, f.x as i32));
        }
        if first || f.y != self.last_y {
            out.push(Ev::abs(yc, f.y as i32));
        }

        self.last_buttons = f.buttons;
        self.last_x = f.x;
        self.last_y = f.y;
        self.seen = true;
        out
    }
}

/// Map a (negative-direction, positive-direction) pair of pressed flags to a hat axis value:
/// `-1` for the negative dir, `+1` for the positive, `0` for neither or both.
fn axis_from(neg: bool, pos: bool) -> i32 {
    match (neg, pos) {
        (true, false) => -1,
        (false, true) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(buttons: u8, x: u16, y: u16) -> Frame {
        Frame { buttons, x, y }
    }

    /// Filter to just the button (EV_KEY) events, for readable assertions.
    fn keys(evs: &[Ev]) -> Vec<(u16, i32)> {
        evs.iter().filter(|e| e.ev_type == codes::EV_KEY).map(|e| (e.code, e.value)).collect()
    }
    fn abses(evs: &[Ev]) -> Vec<(u16, i32)> {
        evs.iter().filter(|e| e.ev_type == codes::EV_ABS).map(|e| (e.code, e.value)).collect()
    }

    #[test]
    fn right_side_first_frame_emits_both_axes_and_held_buttons() {
        let mut d = SideDecoder::new(Side::Right);
        // The RIGHT/east face button held (bit 0x10) + centre-ish sticks.
        let evs = d.apply(f(0x10, 2048, 2050));
        assert_eq!(keys(&evs), vec![(codes::BTN_EAST, 1)], "east pressed at startup");
        assert_eq!(abses(&evs), vec![(codes::ABS_RX, 2048), (codes::ABS_RY, 2050)]);
    }

    /// ⚠ A MIRROR of [`RIGHT_BTN`], not an independent check of it: the cases below are the same
    /// table written twice, so this test can only catch an UNINTENDED change to the mapping — it
    /// cannot tell you the mapping is semantically wrong, because a wrong mapping is made green
    /// simply by editing this copy too. The truth-check against physical position lives in
    /// `tests/face_position_frame.rs`; see `tsp-ozbp.14` for why the distinction mattered.
    #[test]
    fn every_right_button_maps_to_its_ground_truth_code() {
        let cases = [
            (0x01u8, codes::BTN_TR),
            (0x02, codes::BTN_TR2),
            (0x04, codes::BTN_NORTH),
            (0x08, codes::BTN_WEST),
            (0x10, codes::BTN_EAST),
            (0x20, codes::BTN_SOUTH),
            (0x40, codes::BTN_SELECT),
            (0x80, codes::BTN_START),
        ];
        for (mask, code) in cases {
            let mut d = SideDecoder::new(Side::Right);
            d.apply(f(0x00, 2048, 2048)); // establish baseline (no buttons)
            let evs = d.apply(f(mask, 2048, 2048));
            assert_eq!(keys(&evs), vec![(code, 1)], "bit {mask:#04x} → {code:#05x} press");
            let evs = d.apply(f(0x00, 2048, 2048));
            assert_eq!(keys(&evs), vec![(code, 0)], "bit {mask:#04x} → {code:#05x} release");
        }
    }

    #[test]
    fn every_left_button_maps_to_its_ground_truth_code() {
        let cases =
            [(0x01u8, codes::BTN_TL), (0x02, codes::BTN_TL2), (0x80, codes::BTN_MODE)];
        for (mask, code) in cases {
            let mut d = SideDecoder::new(Side::Left);
            d.apply(f(0x00, 2048, 2048));
            let evs = d.apply(f(mask, 2048, 2048));
            assert_eq!(keys(&evs), vec![(code, 1)], "left bit {mask:#04x} → {code:#05x}");
        }
    }

    #[test]
    fn left_dpad_bits_produce_a_hat_not_buttons() {
        let mut d = SideDecoder::new(Side::Left);
        d.apply(f(0x00, 2048, 2048)); // baseline centred
        // Up.
        let evs = d.apply(f(D_UP, 2048, 2048));
        assert_eq!(keys(&evs), vec![], "d-pad never emits a button");
        assert_eq!(abses(&evs), vec![(codes::ABS_HAT0Y, -1)], "Dup → HAT0Y=-1");
        // Down.
        let evs = d.apply(f(D_DOWN, 2048, 2048));
        assert_eq!(abses(&evs), vec![(codes::ABS_HAT0Y, 1)], "Ddown → HAT0Y=+1");
        // Left — HAT0X goes to -1 and the prior HAT0Y returns to centre (X emitted before Y).
        let evs = d.apply(f(D_LEFT, 2048, 2048));
        assert_eq!(abses(&evs), vec![(codes::ABS_HAT0X, -1), (codes::ABS_HAT0Y, 0)]);
        // Right.
        let evs = d.apply(f(D_RIGHT, 2048, 2048));
        assert_eq!(abses(&evs), vec![(codes::ABS_HAT0X, 1)], "Dright → HAT0X=+1");
        // Release to centre.
        let evs = d.apply(f(0x00, 2048, 2048));
        assert_eq!(abses(&evs), vec![(codes::ABS_HAT0X, 0)]);
    }

    #[test]
    fn opposite_dpad_bits_cancel_to_centre() {
        let mut d = SideDecoder::new(Side::Left);
        d.apply(f(0x00, 2048, 2048));
        let evs = d.apply(f(D_LEFT | D_RIGHT, 2048, 2048));
        assert_eq!(abses(&evs), vec![], "left+right → centre, no motion from rest");
    }

    #[test]
    fn left_unused_pro_s_bit_is_ignored() {
        let mut d = SideDecoder::new(Side::Left);
        d.apply(f(0x00, 2048, 2048));
        let evs = d.apply(f(0x40, 2048, 2048)); // the Pro-S-only bit, unused on base
        assert_eq!(evs, vec![], "unused bit 0x40 emits nothing");
    }

    #[test]
    fn sticks_are_edge_triggered_after_the_first_frame() {
        let mut d = SideDecoder::new(Side::Left);
        d.apply(f(0x00, 1000, 2000));
        let evs = d.apply(f(0x00, 1000, 2000));
        assert_eq!(evs, vec![], "unchanged frame → no events");
        let evs = d.apply(f(0x00, 1234, 2000));
        assert_eq!(abses(&evs), vec![(codes::ABS_X, 1234)], "only the moved axis re-emits");
    }

    #[test]
    fn left_and_right_sticks_use_different_axis_codes() {
        let mut l = SideDecoder::new(Side::Left);
        let mut r = SideDecoder::new(Side::Right);
        assert_eq!(abses(&l.apply(f(0, 10, 20))), vec![(codes::ABS_X, 10), (codes::ABS_Y, 20)]);
        assert_eq!(abses(&r.apply(f(0, 30, 40))), vec![(codes::ABS_RX, 30), (codes::ABS_RY, 40)]);
    }
}
