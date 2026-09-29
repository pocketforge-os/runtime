//! The evdev event-type / button / axis code constants this decoder emits.
//!
//! Values are the canonical Linux `input-event-codes.h` numbers, restricted to exactly the
//! controls the A133 pad physically wires (`platform/devices/a133/capabilities.toml` is the
//! authority for which controls the pad presents). Kept as a small self-contained table — the
//! same philosophy as `pf-input-collect::codes` and `pf-input-broker::remap` — so a wrong number
//! is a one-line audit against the kernel ABI, never buried in a struct.
//!
//! # THE TWO FRAMES — read this before touching a face-button code (`tsp-ozbp.14`)
//!
//! An evdev button code carries no statement of which of two *frames* it is expressed in, and
//! the two disagree on this chassis. Every surface in this repo that carries a face-button code
//! names its frame; this module is the origin of the codes, so it defines the vocabulary:
//!
//! - **Frame D — driver-emitted (the wire).** Whatever the underlying driver actually puts on
//!   the evdev node. It is an observation, never a promise; a vendor driver is free to emit
//!   anything. `capabilities.toml`'s `code` field records Frame D.
//! - **Frame C — kernel-canonical positional.** `BTN_SOUTH`/`BTN_EAST`/`BTN_WEST`/`BTN_NORTH`
//!   keyed on the button's PHYSICAL POSITION in the diamond, per the kernel's own gamepad
//!   convention. `pf-input-broker`'s re-emitted device is Frame C, and that is what an app sees.
//!
//! **This module — and therefore everything `pf-input-decode` emits — is FRAME C.** We own this
//! decoder, so its Frame D *is* Frame C by construction: there is no quirk to describe, and the
//! honest thing is to emit canonical codes rather than compensate downstream.
//!
//! ## Why the face codes are named positionally here, and MUST stay that way
//!
//! The kernel aliases the two naming systems onto the same numbers:
//!
//! | position | canonical    | value | kernel alias | glyph on THIS chassis |
//! |----------|--------------|-------|--------------|-----------------------|
//! | south    | `BTN_SOUTH`  | 0x130 | `BTN_A`      | **B**                 |
//! | east     | `BTN_EAST`   | 0x131 | `BTN_B`      | **A**                 |
//! | west     | `BTN_WEST`   | 0x134 | `BTN_Y`      | **Y**                 |
//! | north    | `BTN_NORTH`  | 0x133 | `BTN_X`      | **X**                 |
//!
//! This chassis is NINTENDO-arranged, so the printed glyph and the alias letter agree on
//! west/north and are INVERTED on south/east. Keying a mapping off the printed glyph therefore
//! looks correct exactly half the time — which is how the shipped A/B swap survived a full test
//! suite until `tsp-ozbp.14`. The `BTN_A`/`BTN_B`/`BTN_X`/`BTN_Y` spellings are deliberately
//! **NOT defined in this crate**: a letter is ambiguous between "the glyph" and "the position",
//! and removing the ambiguous spelling is what stops the bug being re-introduced by a plausible
//! one-line edit. Use the positional names; put the glyph in a comment if a reader needs it.

/// `EV_SYN` — report-boundary event type.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY` — key/button event type.
pub const EV_KEY: u16 = 0x01;
/// `EV_ABS` — absolute-axis event type.
pub const EV_ABS: u16 = 0x03;
/// `SYN_REPORT` — commit the current event report.
pub const SYN_REPORT: u16 = 0x00;

// --- face buttons — FRAME C, keyed on PHYSICAL POSITION, never on the printed glyph ----------
// See the module docs for the full frame contract and the alias/glyph table. In one line: the
// button at a given position in the diamond emits that position's canonical code, whatever
// letter is silkscreened on it.
/// The **BOTTOM** face button (`BTN_SOUTH`; kernel alias `BTN_A`; printed **B** on this chassis).
pub const BTN_SOUTH: u16 = 0x130;
/// The **RIGHT** face button (`BTN_EAST`; kernel alias `BTN_B`; printed **A** on this chassis).
pub const BTN_EAST: u16 = 0x131;
/// The **TOP** face button (`BTN_NORTH`; kernel alias `BTN_X`; printed **X** on this chassis).
pub const BTN_NORTH: u16 = 0x133;
/// The **LEFT** face button (`BTN_WEST`; kernel alias `BTN_Y`; printed **Y** on this chassis).
pub const BTN_WEST: u16 = 0x134;

// --- shoulders -------------------------------------------------------------------------------
/// **L1** shoulder.
pub const BTN_TL: u16 = 0x136;
/// **R1** shoulder.
pub const BTN_TR: u16 = 0x137;
// L2/R2 are NOT buttons on this node — see the trigger AXES below (`tsp-f3fm.217`). The
// `BTN_TL2`/`BTN_TR2` spellings are deliberately NOT defined in this crate: this module is the
// vocabulary of what the decoder EMITS, and a defined-but-unemitted trigger button is exactly the
// plausible one-line edit that re-breaks the broker's descriptor capability check.

// --- system buttons --------------------------------------------------------------------------
/// **Select**.
pub const BTN_SELECT: u16 = 0x13a;
/// **Start**.
pub const BTN_START: u16 = 0x13b;
/// **Menu** — the base unit's single guide/menu key (the descriptor's `id="guide"` position).
pub const BTN_MODE: u16 = 0x13c;

// --- axes ------------------------------------------------------------------------------------
/// Left stick X (`ttyS4`).
pub const ABS_X: u16 = 0x00;
/// Left stick Y (`ttyS4`).
pub const ABS_Y: u16 = 0x01;
/// Right stick X (`ttyS3`).
pub const ABS_RX: u16 = 0x03;
/// Right stick Y (`ttyS3`).
pub const ABS_RY: u16 = 0x04;
/// **L2** trigger axis (`ttyS4` bit `0x02`). The a133 descriptor (`platform/devices/a133/
/// capabilities.toml`, `id="ltrig"`) declares `EV_ABS ABS_Z`, range `0..255`,
/// `semantics="binary"` — the Xbox-360 trigger-AXIS shape this node's `045e:028e` identity
/// promises, which the stock vendor inputd also emitted (`tsp-v19s`). The switch is physically
/// binary, so the axis only ever carries [`TRIGGER_RELEASED`] or [`TRIGGER_PRESSED`]
/// (endpoint-only). `pf-input-broker` requires this axis from its source and re-emits it to apps
/// as the button `BTN_TL2` with hysteresis; apps never see this axis.
pub const ABS_Z: u16 = 0x02;
/// **R2** trigger axis (`ttyS3` bit `0x02`); descriptor `id="rtrig"` `EV_ABS ABS_RZ`. Same
/// endpoint-only binary contract as [`ABS_Z`]; the broker re-emits it as `BTN_TR2`.
pub const ABS_RZ: u16 = 0x05;
/// D-pad X (hat: -1 left, +1 right).
pub const ABS_HAT0X: u16 = 0x10;
/// D-pad Y (hat: -1 up, +1 down).
pub const ABS_HAT0Y: u16 = 0x11;

/// The raw stick range: the MCU streams a 12-bit unsigned sample, so `0..=4095`. We report the
/// HONEST raw range and record the observed rest/centre live; scaling + deadzone is the
/// calibration layer's job (`tsp-bwrg`), not this decoder's — do not pre-centre here.
pub const STICK_MIN: i32 = 0;
/// See [`STICK_MIN`].
pub const STICK_MAX: i32 = 4095;

/// Trigger axis minimum (`absinfo.min`) — matches the descriptor's `range.min` for ltrig/rtrig.
pub const TRIGGER_MIN: i32 = 0;
/// Trigger axis maximum (`absinfo.max`) — matches the descriptor's `range.max` for ltrig/rtrig.
pub const TRIGGER_MAX: i32 = 255;
/// The value a released trigger reports: the axis minimum (also the uinput initial value, so a
/// released trigger at startup needs no event).
pub const TRIGGER_RELEASED: i32 = TRIGGER_MIN;
/// The value a pressed trigger reports: the axis maximum (a full-scale press, never partial).
pub const TRIGGER_PRESSED: i32 = TRIGGER_MAX;
