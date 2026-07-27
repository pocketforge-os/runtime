//! **The face-button FRAME regression bar** (`tsp-ozbp.14`).
//!
//! Every other test of the control map is a MIRROR: it writes the decoder's own bit→code table a
//! second time and asserts they agree. That catches an accidental change and nothing else — a
//! semantically WRONG mapping is made green by editing the copy too, which is precisely how a
//! shipped A/B swap survived a full green suite from `tsp-ozbp.9` until it was found on the bench.
//!
//! This file is the truth-check instead. It asserts the decoder against **physical position**,
//! taken from evidence that is independent of the decoder's table, and it is written so that the
//! **pre-fix glyph-keyed mapping FAILS it**.
//!
//! # The defect it pins
//!
//! An evdev face code can be read in two frames (see `pf_input_decode::codes` for the contract):
//! **Frame D**, driver-emitted, and **Frame C**, kernel-canonical positional. Nothing in an
//! `EV_KEY` event says which you are holding. The kernel additionally aliases the positional and
//! lettered names onto the same numbers — `BTN_A == BTN_SOUTH` (0x130), `BTN_B == BTN_EAST`
//! (0x131), `BTN_X == BTN_NORTH` (0x133), `BTN_Y == BTN_WEST` (0x134) — and this chassis is
//! NINTENDO-arranged, printing **B** on south and **A** on east while printing **X** on north and
//! **Y** on west. So a table keyed on the printed GLYPH:
//!
//! - agrees with canonical on west/north **by coincidence** (glyph X sits on north, glyph Y on
//!   west, and the aliases line up), and
//! - is INVERTED on south/east.
//!
//! A uniform error that is right exactly half the time reads as a typo, not as a frame error.
//! That is what made it survivable, and it is why this test asserts all four positions even
//! though only two of them changed.
//!
//! # Where the ground truth comes from — and what is deliberately NOT used
//!
//! **NOT used: the `tsp-ozbp.2` UART byte map.** It records the face bits as bare letters
//! (`0x10`=A, `0x20`=B). A letter is exactly the ambiguity this bead exists to remove — the author
//! may have meant the printed glyph or the canonical position — so anchoring on it would rebuild
//! the defect inside its own regression test.
//!
//! **Used: the position-prompted, owner-confirmed `tsp-bwrg.6` candidate** (`owner-cand.toml`,
//! collected live on the real pad on 2026-07-27 through `pf-collect-ui` reading the decoder's
//! event node). The wizard prompted by PHYSICAL POSITION — highlighting `skin_part=btn_south`,
//! the bottom button on the rendered 3D model — and the owner confirmed on sight that the model
//! was right and the pad's reported codes were wrong. It records, per position, the code the
//! decoder emitted:
//!
//! ```text
//! id = "south" (BOTTOM)  code = BTN_B  = 0x131
//! id = "east"  (RIGHT)   code = BTN_A  = 0x130
//! id = "west"  (LEFT)    code = BTN_Y  = 0x134
//! id = "north" (TOP)     code = BTN_X  = 0x133
//! ```
//!
//! ## Deriving position → MCU bit without ever touching a letter
//!
//! The artifact above gives POSITION → emitted CODE. The pre-fix decoder table (`RIGHT_BTN` at
//! `7cafa5d`, before this bead) gives BIT → emitted CODE: `0x04`→0x133, `0x08`→0x134,
//! `0x10`→0x130, `0x20`→0x131. That table is **injective** over these four codes, so it inverts
//! uniquely, and composing the two yields POSITION → BIT with no letter anywhere in the chain:
//!
//! ```text
//! south  emitted 0x131  and only bit 0x20 emitted 0x131  =>  south  is bit 0x20
//! east   emitted 0x130  and only bit 0x10 emitted 0x130  =>  east   is bit 0x10
//! west   emitted 0x134  and only bit 0x08 emitted 0x134  =>  west   is bit 0x08
//! north  emitted 0x133  and only bit 0x04 emitted 0x133  =>  north  is bit 0x04
//! ```
//!
//! Note what this derivation does and does not depend on. It uses the pre-fix table only as an
//! injective *relabelling* of a wire bit — the physical wiring, which this bead does not change —
//! and takes every position fact from the human-confirmed artifact. It is therefore unaffected by
//! whether that table was semantically right, which is the whole point.
//!
//! The expected codes below are hard-coded **numeric literals from `input-event-codes.h`**, never
//! `codes::` constants: a test that asserts the decoder against the crate's own names could be
//! made green by renaming a constant, which is the mirror failure mode all over again.

use pf_input_decode::codes;
use pf_input_decode::decode::{Ev, Side, SideDecoder};
use pf_input_decode::FrameScanner;

const CENTER: u16 = 2048;

/// One face button: where it physically is, which MCU `byte2` bit fires for it, the code the
/// kernel canonically names for that position, and the code a GLYPH-keyed table emits instead.
struct Face {
    position: &'static str,
    /// The glyph silkscreened on this chassis — for the human reading a failure, never an input
    /// to any assertion.
    glyph: &'static str,
    bit: u8,
    /// Canonical Frame C code, literal from `input-event-codes.h`.
    canonical: u16,
    /// What the pre-fix, glyph-keyed table emitted for this position (the shipped defect).
    glyph_keyed: u16,
}

/// The four face buttons. `bit` is derived in the header; `canonical` is the kernel ABI;
/// `glyph_keyed` is the measured pre-fix behaviour from the `tsp-bwrg.6` artifact.
const FACES: &[Face] = &[
    Face { position: "south (BOTTOM)", glyph: "B", bit: 0x20, canonical: 0x130, glyph_keyed: 0x131 },
    Face { position: "east (RIGHT)", glyph: "A", bit: 0x10, canonical: 0x131, glyph_keyed: 0x130 },
    Face { position: "west (LEFT)", glyph: "Y", bit: 0x08, canonical: 0x134, glyph_keyed: 0x134 },
    Face { position: "north (TOP)", glyph: "X", bit: 0x04, canonical: 0x133, glyph_keyed: 0x133 },
];

fn wire(buttons: u8, x: u16, y: u16) -> [u8; 8] {
    [0xFF, 0x01, buttons, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8, 0xFE]
}

/// Press `bit` on the right cluster from a released baseline and return the `EV_KEY` events, via
/// the real byte-stream scanner + decoder (no kernel, no `/dev/uinput`).
fn press(bit: u8) -> Vec<(u16, i32)> {
    let mut scanner = FrameScanner::new();
    let mut decoder = SideDecoder::new(Side::Right);
    let mut last: Vec<Ev> = Vec::new();
    for frame_bytes in [wire(0x00, CENTER, CENTER), wire(bit, CENTER, CENTER)] {
        for f in scanner.push(&frame_bytes) {
            last = decoder.apply(f);
        }
    }
    last.iter().filter(|e| e.ev_type == codes::EV_KEY).map(|e| (e.code, e.value)).collect()
}

/// **THE BAR.** Each physical face position emits the kernel-canonical code for THAT POSITION.
///
/// Fails under the pre-fix glyph-keyed mapping on south and east — where a table keyed on the
/// printed letter emits the other one's code.
#[test]
fn each_face_position_emits_its_canonical_positional_code() {
    for f in FACES {
        assert_eq!(
            press(f.bit),
            vec![(f.canonical, 1)],
            "{} (printed \"{}\", MCU bit {:#04x}) must emit its CANONICAL positional code {:#05x}. \
             Getting {:#05x} instead means the map is keyed on the printed glyph, not on where \
             the button physically is — see this file's header and pf_input_decode::codes.",
            f.position,
            f.glyph,
            f.bit,
            f.canonical,
            f.glyph_keyed,
        );
    }
}

/// The same defect stated as a prohibition, so the fix cannot be silently reverted: for the two
/// positions where glyph-keying and position-keying DISAGREE, the emitted code must not be the
/// glyph-keyed one.
///
/// This is deliberately redundant with the test above. It is what makes a future "cleanup" that
/// re-derives the table from the chassis letters fail with a message that names the actual
/// mistake, rather than looking like an off-by-one in a constant.
#[test]
fn south_and_east_are_not_glyph_keyed() {
    for f in FACES.iter().filter(|f| f.canonical != f.glyph_keyed) {
        let got = press(f.bit);
        assert_ne!(
            got,
            vec![(f.glyph_keyed, 1)],
            "{} emitted {:#05x}, the code its PRINTED GLYPH \"{}\" names. This chassis is \
             Nintendo-arranged, so the glyph and the position disagree here: the button printed \
             \"{}\" is not the {} position. Key the table on position ({:#05x}).",
            f.position,
            f.glyph_keyed,
            f.glyph,
            f.glyph,
            f.glyph,
            f.canonical,
        );
        assert_eq!(got, vec![(f.canonical, 1)], "{} must emit canonical", f.position);
    }
}

/// The four face positions occupy the four distinct canonical face codes — no position is dropped
/// and none doubles up. A swap that mapped both south and east to 0x130 would satisfy a
/// per-button spot check but is not a valid face-button map.
#[test]
fn the_four_faces_are_a_bijection_onto_the_canonical_face_codes() {
    let mut emitted: Vec<u16> = FACES
        .iter()
        .map(|f| {
            let evs = press(f.bit);
            assert_eq!(evs.len(), 1, "{}: exactly one key event", f.position);
            evs[0].0
        })
        .collect();
    emitted.sort_unstable();
    // BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST — literals, per the header.
    assert_eq!(emitted, vec![0x130, 0x131, 0x133, 0x134], "the four canonical face codes, each once");
}

/// The crate's positional constants really do carry the kernel ABI numbers. Cheap, and it stops a
/// rename from quietly re-pointing a positional name at the wrong value while every mirror test
/// keeps passing.
#[test]
fn positional_constants_match_the_kernel_abi() {
    assert_eq!(codes::BTN_SOUTH, 0x130, "BTN_SOUTH (alias BTN_A)");
    assert_eq!(codes::BTN_EAST, 0x131, "BTN_EAST (alias BTN_B)");
    assert_eq!(codes::BTN_NORTH, 0x133, "BTN_NORTH (alias BTN_X)");
    assert_eq!(codes::BTN_WEST, 0x134, "BTN_WEST (alias BTN_Y)");
}
