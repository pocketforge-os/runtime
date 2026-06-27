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
    ("west", 0x134),  // canonical BTN_WEST — even though the driver emits BTN_X (0x133) here
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
        return AbsInfo { min: -1, max: 1, fuzz: 0, flat: 0 };
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
        Some(a) => AbsInfo { min: a.min, max: a.max, fuzz: a.fuzz, flat: a.flat },
        None => AbsInfo { min: 0, max: 0, fuzz: 0, flat: 0 },
    }
}

/// The descriptor-derived remap: the virtual-device spec + the source→canonical key-code map.
pub struct Remap {
    spec: UinputSpec,
    /// source key code → canonical key code (identity if not normalized).
    key_map: HashMap<u16, u16>,
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

        for inp in &d.inputs {
            let names: Vec<&str> = inp.code.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            match inp.ev_type.as_str() {
                "EV_KEY" => {
                    let src_name = names.first().copied().unwrap_or("");
                    let src = lookup(KEY_CODES, src_name)
                        .ok_or_else(|| RemapError(format!("input '{}': unknown key code {src_name}", inp.id)))?;
                    // Canonical positional code for this id (fallback: keep the source code).
                    let canonical = lookup(CANONICAL_BY_ID, &inp.id).unwrap_or(src);
                    key_map.insert(src, canonical);
                    if !keys.contains(&canonical) {
                        keys.push(canonical);
                    }
                }
                "EV_ABS" => {
                    for name in &names {
                        let code = lookup(ABS_CODES, name)
                            .ok_or_else(|| RemapError(format!("input '{}': unknown abs code {name}", inp.id)))?;
                        let ai = axis_for(inp, name);
                        if !abs.iter().any(|(c, _)| *c == code) {
                            abs.push((code, ai));
                        }
                    }
                }
                other => {
                    return Err(RemapError(format!("input '{}': unsupported ev_type {other}", inp.id)));
                }
            }
        }

        let name = format!("PocketForge Input ({})", d.identity.id);
        let spec = UinputSpec { name, bus, vendor, product, version, keys, abs };
        Ok(Remap { spec, key_map })
    }

    /// The virtual-device spec the broker instantiates.
    pub fn spec(&self) -> &UinputSpec {
        &self.spec
    }

    /// Translate a source key code to its canonical positional code (identity if not normalized).
    pub fn remap_key(&self, source_code: u16) -> u16 {
        self.key_map.get(&source_code).copied().unwrap_or(source_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str) -> Descriptor {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pocketforge/tests/fixtures")
            .join(format!("{id}-capabilities.toml"));
        Descriptor::load(path).expect("load fixture")
    }

    #[test]
    fn sdl_guid_parses_the_xbox360_identity() {
        // 030000005e0400008e02000010010000 → bus 0x0003, vendor 0x045e, product 0x028e, ver 0x0110.
        assert_eq!(parse_sdl_guid("030000005e0400008e02000010010000"), Some((0x0003, 0x045e, 0x028e, 0x0110)));
    }

    #[test]
    fn west_north_driver_quirk_is_normalized() {
        let r = Remap::from_descriptor(&desc("a133")).unwrap();
        // The driver emits BTN_X (0x133) for the physical WEST button → canonical BTN_WEST (0x134).
        assert_eq!(r.remap_key(0x133), 0x134, "BTN_X(west) → BTN_WEST");
        // The driver emits BTN_Y (0x134) for NORTH → canonical BTN_NORTH (0x133).
        assert_eq!(r.remap_key(0x134), 0x133, "BTN_Y(north) → BTN_NORTH");
        // South/east are already canonical (identity).
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
        // Axes: sticks (X/Y/RX/RY), triggers (Z/RZ), hat (HAT0X/Y).
        for code in [0x00, 0x01, 0x03, 0x04, 0x02, 0x05, 0x10, 0x11] {
            assert!(s.abs.iter().any(|(c, _)| *c == code), "abs {code:#x} advertised");
        }
        // Trigger calibration comes from the descriptor range (0..255).
        let z = s.abs.iter().find(|(c, _)| *c == 0x02).unwrap().1;
        assert_eq!((z.min, z.max), (0, 255));
    }

    #[test]
    fn pro_s_only_rows_appear_by_data() {
        let a133 = Remap::from_descriptor(&desc("a133")).unwrap();
        let a523 = Remap::from_descriptor(&desc("a523")).unwrap();
        // a523 adds home (KEY_HOMEPAGE=172) + L3/R3 (BTN_THUMBL/R) — pure descriptor data.
        assert!(!a133.spec().keys.contains(&172), "a133 has no home");
        assert!(a523.spec().keys.contains(&172), "a523 home present");
        assert!(a523.spec().keys.contains(&0x13d) && a523.spec().keys.contains(&0x13e), "a523 L3/R3");
    }
}
