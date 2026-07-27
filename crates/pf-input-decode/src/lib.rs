//! # `pf-input-decode` — the A133 gamepad MCU decoder (`tsp-ozbp.9`)
//!
//! Our owned A133 image ships **no gamepad input device at all**: the pad is an external MCU that
//! streams frames over two RX-only UARTs, and nothing in our stack read them. This crate is the
//! **raw-source layer** that closes that gap — it reads the two UARTs, decodes the frames, and
//! presents ONE standard evdev gamepad via `/dev/uinput`. Everything above it (`pf-input-broker`,
//! `pf-input-collect`, `pf-hwprobe`, `tsp-bwrg`, and any game) reads that evdev node.
//!
//! ## Where it sits in the stack
//!
//! ```text
//!   MCU ── ttyS3 (right) ─┐                        ┌─ pf-input-broker (EVIOCGRAB + descriptor
//!   MCU ── ttyS4 (left)  ─┴─▶ pf-input-decode ─▶ /dev/input/eventN     remap → re-emit)
//!                              (THIS crate)         └─ pf-input-collect / games (read directly)
//! ```
//!
//! It sits **below** `pf-input-broker` and takes no dependency on it: this crate CREATES the node
//! the broker grabs. (The broker's own tests grab a real evdev source; before this crate there was
//! none on the A133.)
//!
//! ## Which frame do the codes on that node carry? — **Frame C** (`tsp-ozbp.14`)
//!
//! Face-button codes exist in two frames that disagree on this chassis — **Frame D**
//! (driver-emitted, what a node happens to put on the wire) and **Frame C** (kernel-canonical
//! positional, `BTN_SOUTH`/`BTN_EAST`/`BTN_WEST`/`BTN_NORTH` by physical position). The evdev
//! code itself says which is which, so **every surface in this repo that carries one names its
//! frame.** This crate emits **Frame C**: we own the decoder, so there is no vendor quirk to
//! describe and the honest thing is to emit canonical codes at the source rather than compensate
//! downstream. Full contract + the alias/glyph table: [`codes`].
//!
//! ## The wire protocol (ground truth `tsp-ozbp.2`)
//!
//! Two RX-only UARTs, **19200 8N1**, each streaming an 8-byte frame **continuously at ~48 fps**
//! (NOT push-on-change): `FF 01 <BTNmask> Xhi Xlo Yhi Ylo FE`, 12-bit sticks. `ttyS3` carries the
//! RIGHT stick + right cluster; `ttyS4` the LEFT stick + left cluster + d-pad + Menu. Full per-bit
//! map + the mapping rationale live in [`decode`].
//!
//! ## One device, not two
//!
//! Although the pad is physically two UARTs, we present **one** evdev gamepad — a gamepad is one
//! device, and every consumer (SDL, the broker's single-source grab, games) expects one node with
//! both sticks. The two UART reader threads feed a single [`uinput::Uinput`]; each thread owns a
//! [`decode::SideDecoder`] for its disjoint half, and only the uinput emit is shared (behind a
//! mutex, so a frame's events + `SYN_REPORT` commit atomically).
//!
//! ## Testability
//!
//! The frame scanner ([`frame`]) and control map ([`decode`]) are **pure logic** with no kernel
//! dependency, exhaustively unit-tested against synthetic frames, plus a `tests/` integration
//! stream that exercises EVERY control (the parity baseline `tsp-ozbp.10` is measured against).

pub mod codes;
pub mod decode;
pub mod frame;
pub mod ioc;
pub mod serial;
pub mod uinput;

pub use decode::{Ev, Side, SideDecoder};
pub use frame::{Frame, FrameScanner};
pub use uinput::{AbsInfo, Uinput, UinputSpec};

/// The compatibility identity the virtual gamepad advertises.
///
/// It presents as the same `"TRIMUI Player1"` / USB `045e:028e` (X360) identity the stock vendor
/// daemon synthesized — deliberately, so it is a true drop-in for the pipeline built around it:
/// the a133 descriptor's `match` rule (`evdev_name="TRIMUI Player1", vid=045e, pid=028e`) finds
/// it, and the shipped `gamecontrollerdb` entry (SDL GUID `030000005e0400008e02000010010000`)
/// maps it as an X360 pad with no per-app config. This is a compatibility *identity*, not vendor
/// code — the whole decoder is owned Rust source; nothing of the vendor's `trimui_inputd` is used.
pub mod identity {
    /// Device name (matches the descriptor `match.evdev_name`).
    pub const NAME: &str = "TRIMUI Player1";
    /// `BUS_USB` — the bus byte the faked X360 identity carries.
    pub const BUS: u16 = 0x03;
    /// Microsoft vendor id (from the shipped X360 gamecontrollerdb GUID).
    pub const VENDOR: u16 = 0x045e;
    /// Xbox-360 wired product id.
    pub const PRODUCT: u16 = 0x028e;
    /// Version word from the GUID (`0x0110`).
    pub const VERSION: u16 = 0x0110;
}

/// Build the full [`UinputSpec`] for the A133 pad: the compatibility identity + every button and
/// axis the base unit physically wires (from [`decode::all_button_codes`] / [`decode::all_axes`]).
pub fn a133_spec() -> UinputSpec {
    let keys = decode::all_button_codes();
    let abs = decode::all_axes()
        .into_iter()
        .map(|(code, min, max)| (code, AbsInfo { min, max, fuzz: 0, flat: 0 }))
        .collect();
    UinputSpec {
        name: identity::NAME.to_string(),
        bus: identity::BUS,
        vendor: identity::VENDOR,
        product: identity::PRODUCT,
        version: identity::VERSION,
        keys,
        abs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a133_spec_advertises_every_control_exactly_once() {
        let s = a133_spec();
        // Identity is the X360 compatibility identity.
        assert_eq!((s.bus, s.vendor, s.product), (0x03, 0x045e, 0x028e));
        // 11 buttons: the four face positions, L1 R1, L2 R2 (as buttons), Select Start Menu.
        let expect_keys = [
            codes::BTN_SOUTH,
            codes::BTN_EAST,
            codes::BTN_NORTH,
            codes::BTN_WEST,
            codes::BTN_TL,
            codes::BTN_TR,
            codes::BTN_TL2,
            codes::BTN_TR2,
            codes::BTN_SELECT,
            codes::BTN_START,
            codes::BTN_MODE,
        ];
        for k in expect_keys {
            assert_eq!(s.keys.iter().filter(|&&c| c == k).count(), 1, "key {k:#05x} advertised once");
        }
        assert_eq!(s.keys.len(), expect_keys.len(), "no extra/missing buttons");
        // Axes: both sticks (raw 12-bit) + the hat.
        for (code, min, max) in [
            (codes::ABS_X, 0, 4095),
            (codes::ABS_Y, 0, 4095),
            (codes::ABS_RX, 0, 4095),
            (codes::ABS_RY, 0, 4095),
            (codes::ABS_HAT0X, -1, 1),
            (codes::ABS_HAT0Y, -1, 1),
        ] {
            let ai = s.abs.iter().find(|(c, _)| *c == code).map(|(_, a)| *a);
            let ai = ai.unwrap_or_else(|| panic!("axis {code:#x} advertised"));
            assert_eq!((ai.min, ai.max), (min, max), "axis {code:#x} range");
        }
        assert_eq!(s.abs.len(), 6, "exactly six axes");
    }
}
