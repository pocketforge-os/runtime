//! The **descriptor-driven action-map remap** — what makes the Pro→Pro-S button delta (and any
//! driver quirk) invisible to the app. The broker re-emits a device whose codes are the CANONICAL
//! POSITIONAL layout (`south`→`BTN_SOUTH`, `west`→`BTN_WEST`, …) regardless of what the underlying
//! driver emits, so an app binding named actions reads identical codes across devices.
//!
//! This is not a no-op: the TrimUI gamepad's X360 driver emits `BTN_X` (0x133, which the kernel
//! ALSO names `BTN_NORTH`) for the physical WEST button and `BTN_Y` (0x134 = `BTN_WEST`) for
//! NORTH — so the descriptor's `id=west code=BTN_X` / `id=north code=BTN_Y` rows make the broker
//! SWAP 0x133↔0x134 onto the canonical `BTN_WEST`/`BTN_NORTH`. The app never sees the driver quirk.
//!
//! Built purely from the descriptor (zero per-device code): a133 and a523 differ only by rows.
//!
//! # THE TWO-FRAME CONTRACT (`tsp-ozbp.14`) — what this module translates BETWEEN
//!
//! A face-button evdev code is meaningless without saying which of two frames it is expressed in,
//! and nothing in an `EV_KEY` event says which. **Frame D (driver-emitted)** is what a driver
//! actually puts on the wire — an observation, not a promise; it is what a descriptor row's
//! `code` field records. **Frame C (kernel-canonical positional)** is `BTN_SOUTH`/`BTN_EAST`/
//! `BTN_WEST`/`BTN_NORTH` keyed on the button's physical position in the diamond. This module is
//! the ONLY D→C boundary in the stack: [`Remap::remap_key`] takes a descriptor row's Frame-D
//! `code` as its input key and emits the [`CANONICAL_BY_ID`] Frame-C code for that row's `id`, so
//! everything upstream of the broker is Frame D and everything downstream is Frame C. The frames
//! coincide on some rows and are inverted on others, which is why "it looks right" is never
//! evidence: on a Nintendo-arranged chassis the kernel aliases (`BTN_X == BTN_NORTH`,
//! `BTN_Y == BTN_WEST`) make a frame error *coincidentally correct* on west/north while inverting
//! south/east. Every surface that carries a code names its frame; when you add one, say which.
//!
//! **Live state of the a133 rows.** `pf-input-decode` — our OWNED decoder, the raw source on this
//! device — emits **Frame C directly** as of `tsp-ozbp.14`: it was keyed on the printed glyph and
//! shipped a south/east inversion, and it was fixed at the source rather than compensated for
//! here. The a133 descriptor's west/north rows still declare the pre-ownership Frame-D codes, so
//! this remap is still a real swap for those two rows; correcting them to canonical (which makes
//! the a133 remap an identity, the honest state once we own the driver) is descriptor territory
//! and is routed separately — do NOT "fix" it by editing [`CANONICAL_BY_ID`], which is the kernel
//! ABI and not ours to bend. The X360-quirk reasoning above still applies verbatim to any device
//! whose driver we do NOT own: describe a quirk you cannot fix, fix one you can.

use std::collections::HashMap;

use pocketforge::descriptor::{Descriptor, Input};

use crate::uinput::{AbsInfo, UinputSpec};

// --- the evdev code name → value table for the codes our descriptors use --------------------
// (A focused table, not all of <linux/input-event-codes.h>; unknown names are a build-order
// error surfaced at remap construction, never a silent mismap.)

/// Canonical button/key code values (Linux `input-event-codes.h`). Note `BTN_X == BTN_NORTH`
/// (0x133) and `BTN_Y == BTN_WEST` (0x134) — the source of the driver-quirk swap.
const KEY_CODES: &[(&str, u16)] = &[
    ("BTN_SOUTH", 0x130),
    ("BTN_A", 0x130),
    ("BTN_EAST", 0x131),
    ("BTN_B", 0x131),
    ("BTN_C", 0x132),
    ("BTN_NORTH", 0x133),
    ("BTN_X", 0x133),
    ("BTN_WEST", 0x134),
    ("BTN_Y", 0x134),
    ("BTN_TL", 0x136),
    ("BTN_TR", 0x137),
    ("BTN_TL2", 0x138),
    ("BTN_TR2", 0x139),
    ("BTN_SELECT", 0x13a),
    ("BTN_START", 0x13b),
    ("BTN_MODE", 0x13c),
    ("BTN_THUMBL", 0x13d),
    ("BTN_THUMBR", 0x13e),
    ("KEY_HOMEPAGE", 172),
];

/// Absolute-axis code values.
const ABS_CODES: &[(&str, u16)] = &[
    ("ABS_X", 0x00),
    ("ABS_Y", 0x01),
    ("ABS_Z", 0x02),
    ("ABS_RX", 0x03),
    ("ABS_RY", 0x04),
    ("ABS_RZ", 0x05),
    ("ABS_HAT0X", 0x10),
    ("ABS_HAT0Y", 0x11),
];

/// The CANONICAL positional key code for a descriptor input id (the layout the app sees). Ids not
/// here keep their source code (identity remap).
const CANONICAL_BY_ID: &[(&str, u16)] = &[
    ("south", 0x130),
    ("east", 0x131),
    ("west", 0x134), // canonical BTN_WEST — even though the driver emits BTN_X (0x133) here
    ("north", 0x133), // canonical BTN_NORTH — even though the driver emits BTN_Y (0x134) here
    ("l1", 0x136),
    ("r1", 0x137),
    ("select", 0x13a),
    ("start", 0x13b),
    ("guide", 0x13c),
    ("home", 172),
    ("l3", 0x13d),
    ("r3", 0x13e),
];

/// The canonical positional BUTTON code a `semantics="binary"` ABS trigger re-emits as, keyed by
/// the trigger's wire ABS code. The two analog-wire/binary-physical triggers (`ltrig`/`ABS_Z`,
/// `rtrig`/`ABS_RZ`; tsp-ozbp.2) present as the Linux gamepad digital lower-trigger buttons
/// `BTN_TL2`/`BTN_TR2` — the natural siblings of `BTN_TL`/`BTN_TR` (l1/r1). A binary ABS row whose
/// code is not here is a descriptor/build error (surfaced at remap construction), never a silent
/// mismap — the same philosophy as the unknown-key/abs lookups above.
const BINARY_TRIGGER_BTN: &[(&str, u16)] = &[
    ("ABS_Z", 0x138),  // ltrig  → BTN_TL2
    ("ABS_RZ", 0x139), // rtrig  → BTN_TR2
];

fn lookup(table: &[(&str, u16)], name: &str) -> Option<u16> {
    table.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// Failure building a remap from a descriptor (an unknown evdev code name in a row).
#[derive(Debug)]
pub struct RemapError(pub String);

impl std::fmt::Display for RemapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remap: {}", self.0)
    }
}

impl std::error::Error for RemapError {}

/// Parse an SDL3 32-hex joystick GUID → `(bus, vendor, product, version)` (LE u16 at the standard
/// offsets), matching `uinput_synth.py::parse_sdl_guid`.
fn parse_sdl_guid(guid: &str) -> Option<(u16, u16, u16, u16)> {
    let b = (0..guid.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(guid.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    if b.len() != 16 {
        return None;
    }
    let u16le = |i: usize| b[i] as u16 | ((b[i + 1] as u16) << 8);
    Some((u16le(0), u16le(4), u16le(8), u16le(12)))
}

fn axis_for(inp: &Input, code_name: &str) -> AbsInfo {
    if code_name.starts_with("ABS_HAT") {
        return AbsInfo {
            min: -1,
            max: 1,
            fuzz: 0,
            flat: 0,
        };
    }
    let ax = if inp.kind == "stick" {
        let codes: Vec<&str> = inp.code.split(',').map(|s| s.trim()).collect();
        if !codes.is_empty() && code_name == codes[0] {
            inp.x
        } else {
            inp.y
        }
    } else {
        inp.range
    };
    match ax {
        Some(a) => AbsInfo {
            min: a.min,
            max: a.max,
            fuzz: a.fuzz,
            flat: a.flat,
        },
        None => AbsInfo {
            min: 0,
            max: 0,
            fuzz: 0,
            flat: 0,
        },
    }
}

/// A physically-binary trigger reported over an analog wire `range`: its ABS events re-emit as an
/// `EV_KEY` press/release on `btn`, switched with hysteresis (`low`/`high` derived from `range`).
#[derive(Debug, Clone, Copy)]
struct BinaryTrigger {
    /// The canonical button code to press/release.
    btn: u16,
    /// Release when the value drops to ≤ this (min + span/4).
    low: i32,
    /// Press when the value rises to ≥ this (min + 3·span/4).
    high: i32,
}

/// What to do with one source `EV_ABS` event — the pure classification the pump applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsAction {
    /// A normal analog axis (`semantics` absent or `"analog"`): re-emit the ABS event unchanged.
    Passthrough,
    /// A binary trigger crossed a threshold: emit `EV_KEY` on `code` with `value` (1 press / 0
    /// release) INSTEAD of the raw ABS.
    Button { code: u16, value: i32 },
    /// A binary trigger whose value stayed inside the hysteresis band (or repeated the current
    /// state): emit nothing (and never the raw ABS).
    None,
}

/// The descriptor-derived remap: the virtual-device spec + the source→canonical key-code map +
/// the binary-trigger (analog-wire→button) classification.
pub struct Remap {
    spec: UinputSpec,
    /// source key code → canonical key code (identity if not normalized).
    key_map: HashMap<u16, u16>,
    /// ABS code → binary-trigger classification (a `semantics="binary"` analog-wire trigger).
    binary: HashMap<u16, BinaryTrigger>,
    /// Live pressed/released state per binary-trigger ABS code (starts released) — the hysteresis
    /// latch, so an intermediate value holds the last state and a threshold cross toggles once.
    binary_state: HashMap<u16, bool>,
    source_keys: Vec<u16>,
    source_abs: Vec<u16>,
}

impl Remap {
    /// Build from a parsed descriptor. The re-emit device advertises the canonical positional
    /// codes + the descriptor's axes; `remap_key` translates the source's driver codes onto them.
    pub fn from_descriptor(d: &Descriptor) -> Result<Remap, RemapError> {
        let (bus, vendor, product, version) = parse_sdl_guid(&d.identity.sdl_guid)
            .ok_or_else(|| RemapError(format!("bad sdl_guid {:?}", d.identity.sdl_guid)))?;

        let mut keys: Vec<u16> = Vec::new();
        let mut abs: Vec<(u16, AbsInfo)> = Vec::new();
        let mut key_map: HashMap<u16, u16> = HashMap::new();
        let mut binary: HashMap<u16, BinaryTrigger> = HashMap::new();
        let mut source_keys = Vec::new();
        let mut source_abs = Vec::new();

        for inp in &d.inputs {
            // SYSTEM controls (class="system", e.g. VOL±) are NOT part of the virtual gamepad
            // (tsp-bwrg.16, owner ruling): the broker must never synthesize them into the pad or
            // hand them to apps as a gamepad binding — describe the whole device, gate system-key
            // access elsewhere. Skip them here so they never reach `spec.keys`/`key_map`/`binary`.
            if inp.is_system() {
                continue;
            }
            let names: Vec<&str> = inp
                .code
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            match inp.ev_type.as_str() {
                "EV_KEY" => {
                    let src_name = names.first().copied().unwrap_or("");
                    let src = lookup(KEY_CODES, src_name).ok_or_else(|| {
                        RemapError(format!("input '{}': unknown key code {src_name}", inp.id))
                    })?;
                    // Canonical positional code for this id (fallback: keep the source code).
                    let canonical = lookup(CANONICAL_BY_ID, &inp.id).unwrap_or(src);
                    if !source_keys.contains(&src) {
                        source_keys.push(src);
                    }
                    key_map.insert(src, canonical);
                    if !keys.contains(&canonical) {
                        keys.push(canonical);
                    }
                }
                "EV_ABS" => {
                    // A physically-binary trigger reported over an analog wire range re-emits as a
                    // BUTTON, not an axis: advertise the button code + record the classification,
                    // and do NOT advertise the ABS axis (it would be a dead, never-moving axis).
                    let is_binary = inp.semantics.as_deref() == Some("binary");
                    for name in &names {
                        let code = lookup(ABS_CODES, name).ok_or_else(|| {
                            RemapError(format!("input '{}': unknown abs code {name}", inp.id))
                        })?;
                        if !source_abs.contains(&code) {
                            source_abs.push(code);
                        }
                        if is_binary {
                            let btn = lookup(BINARY_TRIGGER_BTN, name).ok_or_else(|| {
                                RemapError(format!(
                                    "input '{}': no canonical button for binary abs {name}",
                                    inp.id
                                ))
                            })?;
                            // Thresholds from the descriptor range (default 0..255): press near the
                            // top, release near the bottom, with a wide dead band between. The
                            // signal is physically bistable (endpoints), so the exact fractions are
                            // not sensitive; the two-sided threshold debounces boundary noise.
                            let (min, max) = inp.range.map(|a| (a.min, a.max)).unwrap_or((0, 255));
                            let span = (max - min).max(1);
                            let bt = BinaryTrigger {
                                btn,
                                low: min + span / 4,
                                high: min + span * 3 / 4,
                            };
                            binary.insert(code, bt);
                            if !keys.contains(&btn) {
                                keys.push(btn);
                            }
                        } else {
                            let ai = axis_for(inp, name);
                            if !abs.iter().any(|(c, _)| *c == code) {
                                abs.push((code, ai));
                            }
                        }
                    }
                }
                other => {
                    return Err(RemapError(format!(
                        "input '{}': unsupported ev_type {other}",
                        inp.id
                    )));
                }
            }
        }

        let name = format!("PocketForge Input ({})", d.identity.id);
        let spec = UinputSpec {
            name,
            bus,
            vendor,
            product,
            version,
            keys,
            abs,
        };
        Ok(Remap {
            spec,
            key_map,
            binary,
            binary_state: HashMap::new(),
            source_keys,
            source_abs,
        })
    }

    /// The virtual-device spec the broker instantiates.
    pub fn spec(&self) -> &UinputSpec {
        &self.spec
    }

    pub(crate) fn required_source_keys(&self) -> &[u16] {
        &self.source_keys
    }
    pub(crate) fn required_source_abs(&self) -> &[u16] {
        &self.source_abs
    }

    /// Translate a source key code to its canonical positional code (identity if not normalized).
    pub fn remap_key(&self, source_code: u16) -> u16 {
        self.key_map
            .get(&source_code)
            .copied()
            .unwrap_or(source_code)
    }

    /// Classify one source `EV_ABS` event. A normal analog axis is [`AbsAction::Passthrough`]; a
    /// `semantics="binary"` trigger is latched to a pressed/released state with hysteresis and
    /// re-emitted as an `EV_KEY` button ONLY on a state change ([`AbsAction::Button`]), never as a
    /// raw ABS ([`AbsAction::None`] while inside the dead band or on a repeat).
    pub fn classify_abs(&mut self, abs_code: u16, value: i32) -> AbsAction {
        let bt = match self.binary.get(&abs_code) {
            Some(bt) => *bt,
            None => return AbsAction::Passthrough,
        };
        let was_pressed = self.binary_state.get(&abs_code).copied().unwrap_or(false);
        // Hysteresis latch: cross high → pressed, cross low → released, else hold.
        let now_pressed = if value >= bt.high {
            true
        } else if value <= bt.low {
            false
        } else {
            was_pressed
        };
        if now_pressed == was_pressed {
            return AbsAction::None;
        }
        self.binary_state.insert(abs_code, now_pressed);
        AbsAction::Button {
            code: bt.btn,
            value: now_pressed as i32,
        }
    }

    /// Convert an authoritative post-SYN_DROPPED axis value to virtual state. Unlike normal
    /// event classification this always returns a binary trigger's current logical value.
    pub(crate) fn resync_abs(&mut self, abs_code: u16, value: i32) -> AbsAction {
        let Some(bt) = self.binary.get(&abs_code).copied() else {
            return AbsAction::Passthrough;
        };
        let was_pressed = self.binary_state.get(&abs_code).copied().unwrap_or(false);
        let pressed = if value >= bt.high {
            true
        } else if value <= bt.low {
            false
        } else {
            was_pressed
        };
        self.binary_state.insert(abs_code, pressed);
        AbsAction::Button {
            code: bt.btn,
            value: pressed as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The REAL device descriptor from the `platform` checkout — no vendored copy (`tsp-ozbp.16`).
    /// Panics (never skips) when no checkout is found; see `pocketforge::test_support`.
    fn desc(id: &str) -> Descriptor {
        pocketforge::test_support::descriptor(id)
    }

    #[test]
    fn sdl_guid_parses_the_xbox360_identity() {
        // 030000005e0400008e02000010010000 → bus 0x0003, vendor 0x045e, product 0x028e, ver 0x0110.
        assert_eq!(
            parse_sdl_guid("030000005e0400008e02000010010000"),
            Some((0x0003, 0x045e, 0x028e, 0x0110))
        );
    }

    /// The D→C invariant, asserted against the CHECKED-OUT descriptor rather than against a
    /// hardcoded swap-or-identity verdict (`tsp-ozbp.16`).
    ///
    /// The a133 remap is a REAL SWAP today: platform's descriptor still declares the pre-ownership
    /// Frame-D codes (`id=west code=BTN_X` (0x133), `id=north code=BTN_Y` (0x134)), so the broker
    /// swaps 0x133↔0x134 onto canonical `BTN_WEST`/`BTN_NORTH`. platform PR #92 corrects those two
    /// rows to canonical, at which point the a133 remap becomes an IDENTITY — the honest state once
    /// we own the driver.
    ///
    /// Since the suite floats on `platform@main` (there is no vendored copy — `tsp-ozbp.16`), a
    /// test that hardcoded "it is a swap" would go red the instant #92 merged, with no runtime
    /// commit involved. So it asserts what is true under BOTH descriptors instead: every face row's
    /// Frame-D `code` remaps onto the Frame-C code for that row's `id`. The bijection clause below
    /// is why this is stronger than either hardcoded verdict rather than merely more tolerant — it
    /// catches a HALF-applied correction (west fixed, north not), which is a live-wire mapping bug
    /// that both "assert a swap" and "assert an identity" would sail straight past.
    ///
    /// The remap MECHANISM keeps its own proof in `driver_quirk_swap_still_works_on_a_quirky_driver`
    /// — still needed for every driver we do NOT own.
    #[test]
    fn west_north_driver_quirk_is_normalized() {
        let d = desc("a133");
        let r = Remap::from_descriptor(&d).unwrap();

        let mut canonical_seen = Vec::new();
        for id in ["south", "east", "west", "north"] {
            let row = d
                .inputs
                .iter()
                .find(|i| i.id == id)
                .unwrap_or_else(|| panic!("a133 descriptor has no {id} face row"));
            let driver_code = lookup(KEY_CODES, &row.code)
                .unwrap_or_else(|| panic!("{id}: unknown evdev code name {:?}", row.code));
            let canonical = lookup(CANONICAL_BY_ID, id).expect("face ids are all canonical");
            assert_eq!(
                r.remap_key(driver_code),
                canonical,
                "{id}: driver code {driver_code:#x} ({}) must remap to canonical {canonical:#x}",
                row.code,
            );
            canonical_seen.push(canonical);
        }

        // The four face buttons must land on FOUR DISTINCT canonical codes. A descriptor that
        // corrected `west` without correcting `north` (or vice versa) would collide two positions
        // onto one code — every app would read the wrong button — and would still satisfy the
        // per-row assertion above for the row that was changed.
        canonical_seen.sort_unstable();
        canonical_seen.dedup();
        assert_eq!(
            canonical_seen,
            vec![0x130, 0x131, 0x133, 0x134],
            "face remap is a bijection"
        );
    }

    /// The SWAP mechanism itself, on an explicitly synthetic quirky-driver descriptor.
    ///
    /// `west_north_driver_quirk_is_normalized` is deliberately regime-agnostic, so on its own it
    /// would still pass if the remap silently degraded to a pass-through and every descriptor
    /// happened to be canonical. This keeps a device whose driver reports west/north inverted —
    /// the X360 quirk, and the shape of any driver we do NOT own — so a real swap is always
    /// exercised regardless of what platform's owned descriptors say.
    #[test]
    fn driver_quirk_swap_still_works_on_a_quirky_driver() {
        let quirky = Descriptor::from_toml(
            r#"
[identity]
id = "synthquirk"
manufacturer = "PocketForge"
model = "Quirky X360 Driver Rig (synthetic test descriptor)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"
[[inputs]]
id = "east"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_B"
[[inputs]]
id = "west"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_X"
[[inputs]]
id = "north"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_Y"
"#,
        )
        .expect("parse synthetic quirky descriptor");
        let r = Remap::from_descriptor(&quirky).unwrap();
        // Driver emits BTN_X (0x133) for physical WEST → canonical BTN_WEST (0x134), and BTN_Y
        // (0x134) for NORTH → canonical BTN_NORTH (0x133). A genuine crossover, both ways.
        assert_eq!(r.remap_key(0x133), 0x134, "BTN_X(west) → BTN_WEST");
        assert_eq!(r.remap_key(0x134), 0x133, "BTN_Y(north) → BTN_NORTH");
        // South/east are already canonical on this driver (identity) — the swap is targeted.
        assert_eq!(r.remap_key(0x130), 0x130);
        assert_eq!(r.remap_key(0x131), 0x131);
    }

    #[test]
    fn spec_advertises_canonical_codes_and_axes() {
        let r = Remap::from_descriptor(&desc("a133")).unwrap();
        let s = r.spec();
        assert_eq!((s.bus, s.vendor, s.product), (0x0003, 0x045e, 0x028e));
        // Canonical WEST/NORTH are advertised (not the driver's raw assignment).
        assert!(s.keys.contains(&0x134), "BTN_WEST advertised");
        assert!(s.keys.contains(&0x133), "BTN_NORTH advertised");
        // Analog axes advertised: sticks (X/Y/RX/RY), hat (HAT0X/Y).
        for code in [0x00, 0x01, 0x03, 0x04, 0x10, 0x11] {
            assert!(
                s.abs.iter().any(|(c, _)| *c == code),
                "abs {code:#x} advertised"
            );
        }
        // The ltrig/rtrig are semantics="binary" → advertised as BUTTONS (BTN_TL2/BTN_TR2),
        // NOT as ABS_Z/ABS_RZ axes (a dead axis would mislead the app).
        assert!(
            s.keys.contains(&0x138),
            "BTN_TL2 (ltrig) advertised as a button"
        );
        assert!(
            s.keys.contains(&0x139),
            "BTN_TR2 (rtrig) advertised as a button"
        );
        assert!(
            !s.abs.iter().any(|(c, _)| *c == 0x02),
            "ABS_Z not advertised (binary trigger)"
        );
        assert!(
            !s.abs.iter().any(|(c, _)| *c == 0x05),
            "ABS_RZ not advertised (binary trigger)"
        );
    }

    #[test]
    fn binary_trigger_sweep_re_emits_a_button_with_hysteresis() {
        // The a133 ltrig (ABS_Z, semantics="binary", range 0..255) must re-emit as BTN_TL2.
        let mut r = Remap::from_descriptor(&desc("a133")).unwrap();
        // Thresholds derived from range 0..255: press ≥ 191, release ≤ 63.
        // Sweep 0 → max: nothing until we cross high, then exactly one press.
        assert_eq!(
            r.classify_abs(0x02, 0),
            AbsAction::None,
            "at rest → no event"
        );
        assert_eq!(
            r.classify_abs(0x02, 100),
            AbsAction::None,
            "dead band → no event, NO raw ABS"
        );
        assert_eq!(
            r.classify_abs(0x02, 190),
            AbsAction::None,
            "just below high → still no press"
        );
        assert_eq!(
            r.classify_abs(0x02, 191),
            AbsAction::Button {
                code: 0x138,
                value: 1
            },
            "cross high → BTN_TL2 press"
        );
        assert_eq!(
            r.classify_abs(0x02, 255),
            AbsAction::None,
            "held pressed → no repeat"
        );
        // Sweep back down: hysteresis holds pressed through the dead band, releases only ≤ low.
        assert_eq!(
            r.classify_abs(0x02, 100),
            AbsAction::None,
            "dead band on the way down → hold"
        );
        assert_eq!(
            r.classify_abs(0x02, 64),
            AbsAction::None,
            "just above low → still pressed"
        );
        assert_eq!(
            r.classify_abs(0x02, 63),
            AbsAction::Button {
                code: 0x138,
                value: 0
            },
            "cross low → BTN_TL2 release"
        );
        assert_eq!(
            r.classify_abs(0x02, 0),
            AbsAction::None,
            "released → no repeat"
        );
        // The a133 rtrig (ABS_RZ) is an independent binary trigger → BTN_TR2.
        assert_eq!(
            r.classify_abs(0x05, 255),
            AbsAction::Button {
                code: 0x139,
                value: 1
            },
            "rtrig cross high → BTN_TR2 press"
        );
    }

    #[test]
    fn analog_axis_passes_through_unchanged() {
        // Regression: a NON-binary ABS axis (the a133 lstick ABS_X, no semantics) always passes
        // through — the binary reclassification must not touch analog axes.
        let mut r = Remap::from_descriptor(&desc("a133")).unwrap();
        for v in [-32768, 0, 128, 32767] {
            assert_eq!(
                r.classify_abs(0x00, v),
                AbsAction::Passthrough,
                "ABS_X analog passthrough"
            );
        }
        // And a whole device with NO binary rows classifies every trigger axis as passthrough.
        // This half used to run on the a523 — but platform now describes BOTH shipping devices'
        // L2/R2 as `semantics = "binary"` (SPIKE-0 2026-07-11: endpoint-only full-swing ABS_Z/RZ,
        // no proportional travel), so the "analog trigger" it relied on only existed in the stale
        // vendored copy. The shape is real for other hardware, so it gets an honestly SYNTHETIC
        // device rather than a device pretending to have travel it does not (tsp-ozbp.16).
        let mut analog =
            Remap::from_descriptor(&pocketforge::test_support::analog_trigger_descriptor())
                .unwrap();
        assert_eq!(
            analog.classify_abs(0x02, 255),
            AbsAction::Passthrough,
            "analog ABS_Z"
        );
        assert_eq!(
            analog.classify_abs(0x05, 255),
            AbsAction::Passthrough,
            "analog ABS_RZ"
        );
    }

    #[test]
    fn pro_s_only_rows_appear_by_data() {
        let a133 = Remap::from_descriptor(&desc("a133")).unwrap();
        let a523 = Remap::from_descriptor(&desc("a523")).unwrap();
        // a523 adds home (KEY_HOMEPAGE=172) + L3/R3 (BTN_THUMBL/R) — pure descriptor data.
        assert!(!a133.spec().keys.contains(&172), "a133 has no home");
        assert!(a523.spec().keys.contains(&172), "a523 home present");
        assert!(
            a523.spec().keys.contains(&0x13d) && a523.spec().keys.contains(&0x13e),
            "a523 L3/R3"
        );
    }

    /// SYSTEM controls are excluded from the virtual gamepad (tsp-bwrg.16, owner ruling pt 1/3):
    /// a `class = "system"` row (VOL±) is a real device control but must NEVER be synthesized into
    /// the pad or reach an app as a gamepad binding — access is gated elsewhere, not by omitting the
    /// row from the descriptor. PROVEN-TO-FAIL: drop the `is_system()` skip in `from_descriptor` and
    /// this goes RED — `KEY_VOLUMEUP` is not a gamepad key code, so the un-skipped row makes
    /// `from_descriptor` error (`unknown key code KEY_VOLUMEUP`) and the `.unwrap()` panics.
    #[test]
    fn system_class_rows_are_excluded_from_the_virtual_gamepad() {
        let d = Descriptor::from_toml(
            r#"
[identity]
id = "systest"
manufacturer = "PocketForge"
model = "System-key test rig (synthetic)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"
[[inputs]]
id = "vol_up"
kind = "button"
ev_type = "EV_KEY"
code = "KEY_VOLUMEUP"
class = "system"
source = "sunxi-keyboard"
[[inputs]]
id = "vol_down"
kind = "button"
ev_type = "EV_KEY"
code = "KEY_VOLUMEDOWN"
class = "system"
source = "sunxi-keyboard"
"#,
        )
        .unwrap();
        let r = Remap::from_descriptor(&d).unwrap();
        let keys = &r.spec().keys;
        assert!(
            keys.contains(&0x130),
            "the real gamepad button (BTN_A/south) IS advertised"
        );
        // KEY_VOLUMEUP=115(0x73), KEY_VOLUMEDOWN=114(0x72) must NOT be synthesized as gamepad keys.
        assert!(
            !keys.contains(&0x73) && !keys.contains(&0x72),
            "system VOL± must NOT leak into the virtual gamepad keys: {keys:?}"
        );
        assert_eq!(
            keys.len(),
            1,
            "only the one non-system button is advertised: {keys:?}"
        );
        // A system code is never remapped into the pad (identity fallthrough, never a mapping entry).
        assert_eq!(r.remap_key(0x73), 0x73, "VOL+ has no gamepad remap entry");
    }
}
