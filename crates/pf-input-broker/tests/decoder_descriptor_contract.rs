//! The **decoder ↔ descriptor capability contract** (`tsp-f3fm.217`).
//!
//! On the first a133 image with a working pad chain, `pf-input-broker` refused its source and
//! crash-looped with `opened source lacks descriptor-required capabilities`: the a133 descriptor
//! declares `ltrig`/`rtrig` as `EV_ABS ABS_Z`/`ABS_RZ` (`semantics = "binary"`, `0..255`), while
//! `pf-input-decode` advertised `BTN_TL2`/`BTN_TR2` and no trigger axes. Every suite was green,
//! because nothing ran the broker's check against what the decoder really advertises.
//!
//! This test does exactly that, with no hand-written capability list on either side:
//!
//! - **Source side** — the capability set is FOLDED from [`pf_input_decode::setup_bits`] over
//!   [`pf_input_decode::a133_spec`]: the same bit list `Uinput::create` iterates to issue its
//!   `UI_SET_*BIT` ioctls, for the same spec the daemon passes it. Change the decoder's
//!   advertised set and this side changes with it.
//! - **Check** — [`missing_source_capabilities`], the function the daemon's `validate_source` and
//!   `discover` call, over a [`Remap`] built by the daemon's own `Remap::from_descriptor`.
//! - **Descriptor side** — the real a133 `capabilities.toml`, read from the `platform` checkout
//!   the whole suite reads (`pocketforge::test_support`; CI's `scripts/ci-container.sh` requires
//!   it). It is deliberately NOT copied in as a fixture: `pocketforge/tests/taxonomy.rs`
//!   `no_vendored_descriptor_copy_exists` (tsp-ozbp.16) forbids a second copy, because the last
//!   one drifted. Instead the test PRINTS the sha256 of the exact file it evaluated, and whether
//!   it is the file staged on the tsp-f3fm.215 image (`98305bae…`, platform `54ba5301`), so every
//!   CI log names the descriptor this guard scored.
//!
//! Negative controls prove the check can fail: the pre-fix decoder shape (`BTN_TL2`/`BTN_TR2`,
//! no `ABS_Z`/`ABS_RZ`) and every single-input removal are each reported, by code.

use std::collections::BTreeSet;
use std::io;

use pf_input_broker::remap::AbsAction;
use pf_input_broker::{
    missing_source_capabilities, MissingCapabilities, Remap, SourceCapabilities,
};
use pf_input_decode::decode::{Side, SideDecoder};
use pf_input_decode::{a133_spec, setup_bits, Frame, SetBit, UinputSpec};
use pocketforge::descriptor::Descriptor;
use sha2::{Digest, Sha256};

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const ABS_Z: u16 = 0x02;
const ABS_RZ: u16 = 0x05;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;

/// sha256 of `platform/devices/a133/capabilities.toml` at platform `54ba5301` — the descriptor
/// staged on the tsp-f3fm.215 bench image, where the broker refused the pre-fix decoder.
const STAGED_A133_SHA256: &str = "98305bae7fe0529980c0275af76a4110d870783a03486e24b2596ea44200184e";

/// The `EVIOCGBIT` view of a uinput node, folded from the setup bits its creator issues — the
/// kernel's bitmaps are exactly the set of `UI_SET_*BIT` calls made before `UI_DEV_CREATE`.
struct UinputNode {
    ev: BTreeSet<u16>,
    key: BTreeSet<u16>,
    abs: BTreeSet<u16>,
}

impl UinputNode {
    fn from_spec(spec: &UinputSpec) -> UinputNode {
        let mut node = UinputNode {
            ev: BTreeSet::new(),
            key: BTreeSet::new(),
            abs: BTreeSet::new(),
        };
        for bit in setup_bits(spec) {
            match bit {
                SetBit::Ev(c) => node.ev.insert(c),
                SetBit::Key(c) => node.key.insert(c),
                SetBit::Abs(c) => node.abs.insert(c),
            };
        }
        node
    }
}

impl SourceCapabilities for UinputNode {
    // Stricter than raw EVIOCGBIT: a code counts only if its event type bit is set too.
    fn supports(&self, event_type: u16, codes: &[u16]) -> io::Result<bool> {
        let set = match event_type {
            EV_KEY => &self.key,
            EV_ABS => &self.abs,
            _ => return Ok(false),
        };
        Ok(self.ev.contains(&event_type) && codes.iter().all(|c| set.contains(c)))
    }
}

/// The real a133 descriptor from the platform checkout (panics, never skips, when absent).
fn a133_descriptor() -> Descriptor {
    pocketforge::test_support::descriptor("a133")
}

fn remap(d: &Descriptor) -> Remap {
    Remap::from_descriptor(d).expect("the broker builds its remap from the a133 descriptor")
}

fn missing(spec: &UinputSpec, d: &Descriptor) -> MissingCapabilities {
    missing_source_capabilities(&UinputNode::from_spec(spec), &remap(d)).expect("pure check")
}

/// The decoder as it shipped before tsp-f3fm.217: L2/R2 as buttons, no trigger axes.
fn pre_fix_decoder_spec() -> UinputSpec {
    let mut spec = a133_spec();
    spec.abs.retain(|(c, _)| *c != ABS_Z && *c != ABS_RZ);
    spec.keys.extend([BTN_TL2, BTN_TR2]);
    spec
}

/// THE GUARD: the broker's own check, run against the decoder's real advertised set, over the
/// real a133 descriptor, finds nothing missing — the broker accepts pf-gamepad. The descriptor's
/// identity is printed (not asserted, so an unrelated platform edit cannot turn this red while the
/// contract still holds): a gate must quote what it scored.
#[test]
fn broker_accepts_the_decoders_real_capabilities_for_the_a133_descriptor() {
    let path = pocketforge::test_support::try_descriptor_path("a133").expect("platform checkout");
    let bytes = std::fs::read(&path).expect("read a133 descriptor");
    let sha: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!(
        "decoder_descriptor_contract: a133_descriptor={} sha256={sha} staged_tsp_f3fm_215={}",
        path.display(),
        sha == STAGED_A133_SHA256
    );
    let m = missing(&a133_spec(), &a133_descriptor());
    assert!(m.is_empty(), "broker would refuse pf-gamepad: missing {m}");
    println!("decoder_descriptor_contract: missing={m} (empty = broker accepts pf-gamepad)");
}

/// NEGATIVE CONTROL 1 — the pre-fix decoder (BTN_TL2/BTN_TR2, no ABS_Z/ABS_RZ) FAILS, and the
/// check names exactly the two trigger axes: the tsp-f3fm.215 bench refusal, reproduced.
#[test]
fn negative_control_the_pre_fix_button_trigger_decoder_is_refused() {
    let m = missing(&pre_fix_decoder_spec(), &a133_descriptor());
    assert_eq!(
        m,
        MissingCapabilities {
            keys: vec![],
            abs: vec![ABS_Z, ABS_RZ],
        },
        "the pre-fix decoder must be refused for exactly the ltrig/rtrig axes"
    );
}

/// NEGATIVE CONTROL 2 — removing ANY single required input from the decoder's spec FAILS the
/// check, naming that input. Iterates the broker's whole requirement set, so no required input
/// can be silently optional.
#[test]
fn negative_control_removing_any_required_input_is_refused() {
    let d = a133_descriptor();
    let r = remap(&d);
    assert!(!r.required_source_keys().is_empty() && !r.required_source_abs().is_empty());
    for &code in r.required_source_keys() {
        let mut spec = a133_spec();
        spec.keys.retain(|&k| k != code);
        let m = missing(&spec, &d);
        assert_eq!(
            m.keys,
            vec![code],
            "dropping key {code:#05x} must be refused"
        );
        assert!(m.abs.is_empty());
    }
    for &code in r.required_source_abs() {
        let mut spec = a133_spec();
        spec.abs.retain(|(c, _)| *c != code);
        let m = missing(&spec, &d);
        assert_eq!(
            m.abs,
            vec![code],
            "dropping axis {code:#05x} must be refused"
        );
        assert!(m.keys.is_empty());
    }
}

/// The requirement set is PAD-sourced only: `class = "system"` rows (VOL±, which carry
/// `source = "sunxi-keyboard"`, a different node) never enter it. The broker filters on `class`,
/// so also pin that every row naming a foreign `source` is a system row.
#[test]
fn broker_requires_only_pad_sourced_inputs() {
    let d = a133_descriptor();
    for inp in &d.inputs {
        if inp.source.is_some() {
            assert!(
                inp.is_system(),
                "row {} names a non-pad source {:?} but is not class=system, so the broker \
                 would demand it from pf-gamepad",
                inp.id,
                inp.source
            );
        }
    }
    let vol: Vec<_> = d
        .inputs
        .iter()
        .filter(|i| i.id.starts_with("vol_"))
        .collect();
    assert_eq!(vol.len(), 2, "descriptor carries vol_up/vol_down");
    assert!(vol
        .iter()
        .all(|i| i.is_system() && i.source.as_deref() == Some("sunxi-keyboard")));
    let r = remap(&d);
    for code in [KEY_VOLUMEUP, KEY_VOLUMEDOWN] {
        assert!(
            !r.required_source_keys().contains(&code),
            "VOL key {code} must not be required from the pad"
        );
    }
}

/// The decoder's trigger absinfo equals the descriptor's `range` for each binary trigger row, so
/// the decoder's endpoint values straddle the broker's hysteresis thresholds (derived from that
/// range) by construction.
#[test]
fn decoder_trigger_absinfo_matches_the_descriptor_range() {
    let d = a133_descriptor();
    let spec = a133_spec();
    for (id, code) in [("ltrig", ABS_Z), ("rtrig", ABS_RZ)] {
        let row = d.inputs.iter().find(|i| i.id == id).expect("trigger row");
        assert_eq!(row.semantics.as_deref(), Some("binary"));
        let range = row.range.expect("binary trigger declares a range");
        let ai = spec
            .abs
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, a)| *a)
            .unwrap_or_else(|| panic!("decoder advertises {id} axis {code:#x}"));
        assert_eq!(
            (ai.min, ai.max, ai.fuzz, ai.flat),
            (range.min, range.max, range.fuzz, range.flat),
            "{id} absinfo"
        );
    }
}

/// End to end, hermetic: a physical L2/R2 press decoded by the REAL decoder, fed through the
/// broker's REAL remap, reaches the app as `BTN_TL2`/`BTN_TR2` press then release — the codes
/// Poolsuite's `input/pf-contract.json` binds to ChannelPrev/ChannelNext. The app-visible device
/// advertises those buttons and not the raw axes.
#[test]
fn a_decoded_trigger_press_reaches_the_app_as_the_trigger_button() {
    let d = a133_descriptor();
    let mut r = remap(&d);
    assert!(r.spec().keys.contains(&BTN_TL2) && r.spec().keys.contains(&BTN_TR2));
    assert!(!r
        .spec()
        .abs
        .iter()
        .any(|(c, _)| *c == ABS_Z || *c == ABS_RZ));

    for (side, app_code) in [(Side::Left, BTN_TL2), (Side::Right, BTN_TR2)] {
        let mut dec = SideDecoder::new(side);
        let frame = |buttons: u8| Frame {
            buttons,
            x: 2048,
            y: 2048,
        };
        dec.apply(frame(0x00));
        let mut app = Vec::new();
        for buttons in [0x02u8, 0x02, 0x00] {
            for ev in dec.apply(frame(buttons)) {
                if ev.ev_type == EV_ABS {
                    match r.classify_abs(ev.code, ev.value) {
                        AbsAction::Button { code, value } => app.push((code, value)),
                        AbsAction::Passthrough => panic!("trigger axis {:#x} leaked raw", ev.code),
                        AbsAction::None => {}
                    }
                }
            }
        }
        assert_eq!(
            app,
            vec![(app_code, 1), (app_code, 0)],
            "{side:?} trigger → app button press/release"
        );
    }
}
