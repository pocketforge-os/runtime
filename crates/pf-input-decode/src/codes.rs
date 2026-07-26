//! The evdev event-type / button / axis code constants this decoder emits.
//!
//! Values are the canonical Linux `input-event-codes.h` numbers, restricted to exactly the
//! controls the A133 pad physically wires (`platform/devices/a133/capabilities.toml` is the
//! authority for which codes the pad presents). Kept as a small self-contained table — the same
//! philosophy as `pf-input-collect::codes` and `pf-input-broker::remap` — so a wrong number is a
//! one-line audit against the kernel ABI, never buried in a struct.

/// `EV_SYN` — report-boundary event type.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY` — key/button event type.
pub const EV_KEY: u16 = 0x01;
/// `EV_ABS` — absolute-axis event type.
pub const EV_ABS: u16 = 0x03;
/// `SYN_REPORT` — commit the current event report.
pub const SYN_REPORT: u16 = 0x00;

// --- face buttons (positional codes; physical label == the like-named BTN_*) -----------------
// The a133 descriptor declares these as the pad's WIRE codes (south=BTN_A, east=BTN_B,
// west=BTN_X, north=BTN_Y). We emit them verbatim: the physical A/B/X/Y button emits its
// identically-named evdev code. See the crate docs for why we do NOT replicate the vendor
// X360 driver's west↔north code quirk (we are fresh owned source, not that driver).
/// Physical **A** button.
pub const BTN_A: u16 = 0x130;
/// Physical **B** button.
pub const BTN_B: u16 = 0x131;
/// Physical **X** button.
pub const BTN_X: u16 = 0x133;
/// Physical **Y** button.
pub const BTN_Y: u16 = 0x134;

// --- shoulders + triggers --------------------------------------------------------------------
/// **L1** shoulder.
pub const BTN_TL: u16 = 0x136;
/// **R1** shoulder.
pub const BTN_TR: u16 = 0x137;
/// **L2** — physically BINARY on this pad, so emitted as the digital lower-trigger button
/// (matches `pf-input-broker`'s `semantics="binary"` → `BTN_TL2` mapping — agree, don't reinvent).
pub const BTN_TL2: u16 = 0x138;
/// **R2** — physically BINARY, emitted as the digital lower-trigger button (`BTN_TR2`).
pub const BTN_TR2: u16 = 0x139;

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
