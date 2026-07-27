//! Synthetic-frame parity baseline — the whole decode path (byte stream → frame scanner →
//! control map) exercised against EVERY control with no kernel and no `/dev/uinput`. This is the
//! code form of the `evtest` transcript the bead requires, and the parity baseline `tsp-ozbp.10`
//! is measured against. If a physical control's mapping ever silently changes, a row here breaks.

use pf_input_decode::codes;
use pf_input_decode::decode::{Ev, Side, SideDecoder};
use pf_input_decode::FrameScanner;

const CENTER: u16 = 2048;

/// Build one 8-byte wire frame.
fn wire(buttons: u8, x: u16, y: u16) -> [u8; 8] {
    [0xFF, 0x01, buttons, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8, 0xFE]
}

/// Drive a side end-to-end through the real byte-stream scanner + decoder, returning the events
/// for the LAST frame in `sequence` (each earlier frame just advances state) — so a test reads
/// "press X, then what came out".
fn run(side: Side, sequence: &[[u8; 8]]) -> Vec<Ev> {
    let mut scanner = FrameScanner::new();
    let mut decoder = SideDecoder::new(side);
    let mut last = Vec::new();
    for frame_bytes in sequence {
        for f in scanner.push(frame_bytes) {
            last = decoder.apply(f);
        }
    }
    last
}

fn keys(evs: &[Ev]) -> Vec<(u16, i32)> {
    evs.iter().filter(|e| e.ev_type == codes::EV_KEY).map(|e| (e.code, e.value)).collect()
}
fn abses(evs: &[Ev]) -> Vec<(u16, i32)> {
    evs.iter().filter(|e| e.ev_type == codes::EV_ABS).map(|e| (e.code, e.value)).collect()
}

/// EVERY right-cluster button (ttyS3), press then release, through the real scanner + decoder.
///
/// ⚠ For the four FACE rows this is a MIRROR of the decoder's own table, not an independent
/// check of it (`tsp-ozbp.14`): the expectation is the same mapping written a second time, so it
/// catches an unintended change but can never catch a *wrong* mapping — making a wrong mapping
/// green needs only the same edit here. It is also why the pre-fix labels read "A"/"B": they
/// named the printed glyph, so a genuine correction surfaced as `A press → 0x130 ... left: 305`
/// and looked like the fix was the bug. Rows are labelled by POSITION now, and the truth-check
/// against physical position lives in `face_position_frame.rs`.
#[test]
fn right_cluster_every_button() {
    let table = [
        (0x01u8, codes::BTN_TR, "R1"),
        (0x02, codes::BTN_TR2, "R2"),
        (0x04, codes::BTN_NORTH, "north (top)"),
        (0x08, codes::BTN_WEST, "west (left)"),
        (0x10, codes::BTN_EAST, "east (right)"),
        (0x20, codes::BTN_SOUTH, "south (bottom)"),
        (0x40, codes::BTN_SELECT, "Select"),
        (0x80, codes::BTN_START, "Start"),
    ];
    for (mask, code, label) in table {
        // baseline (no buttons) → press → release.
        let press = run(Side::Right, &[wire(0x00, CENTER, CENTER), wire(mask, CENTER, CENTER)]);
        assert_eq!(keys(&press), vec![(code, 1)], "{label} press → {code:#05x}");
        let release = run(
            Side::Right,
            &[wire(0x00, CENTER, CENTER), wire(mask, CENTER, CENTER), wire(0x00, CENTER, CENTER)],
        );
        assert_eq!(keys(&release), vec![(code, 0)], "{label} release");
    }
}

/// EVERY left-cluster button (ttyS4): L1, L2, Menu.
#[test]
fn left_cluster_every_button() {
    let table = [
        (0x01u8, codes::BTN_TL, "L1"),
        (0x02, codes::BTN_TL2, "L2"),
        (0x80, codes::BTN_MODE, "Menu"),
    ];
    for (mask, code, label) in table {
        let press = run(Side::Left, &[wire(0x00, CENTER, CENTER), wire(mask, CENTER, CENTER)]);
        assert_eq!(keys(&press), vec![(code, 1)], "{label} press → {code:#05x}");
    }
}

/// The full d-pad, as a hat, in all four directions + diagonals + release.
#[test]
fn dpad_hat_all_directions() {
    let base = wire(0x00, CENTER, CENTER);
    // Up.
    assert_eq!(abses(&run(Side::Left, &[base, wire(0x04, CENTER, CENTER)])), vec![(codes::ABS_HAT0Y, -1)]);
    // Down.
    assert_eq!(abses(&run(Side::Left, &[base, wire(0x20, CENTER, CENTER)])), vec![(codes::ABS_HAT0Y, 1)]);
    // Left.
    assert_eq!(abses(&run(Side::Left, &[base, wire(0x08, CENTER, CENTER)])), vec![(codes::ABS_HAT0X, -1)]);
    // Right.
    assert_eq!(abses(&run(Side::Left, &[base, wire(0x10, CENTER, CENTER)])), vec![(codes::ABS_HAT0X, 1)]);
    // Diagonal up-right → both axes.
    let ur = abses(&run(Side::Left, &[base, wire(0x04 | 0x10, CENTER, CENTER)]));
    assert!(ur.contains(&(codes::ABS_HAT0Y, -1)) && ur.contains(&(codes::ABS_HAT0X, 1)), "up-right diagonal");
    // Release from up → HAT0Y back to 0.
    let rel = abses(&run(Side::Left, &[base, wire(0x04, CENTER, CENTER), base]));
    assert_eq!(rel, vec![(codes::ABS_HAT0Y, 0)]);
}

/// Both sticks: full-scale extremes on each axis, using the raw 12-bit range and the correct
/// per-side axis codes. Left = ABS_X/ABS_Y (ttyS4), Right = ABS_RX/ABS_RY (ttyS3).
#[test]
fn both_sticks_full_scale() {
    // Left stick to (min, max).
    let l = abses(&run(Side::Left, &[wire(0x00, CENTER, CENTER), wire(0x00, 0, 4095)]));
    assert_eq!(l, vec![(codes::ABS_X, 0), (codes::ABS_Y, 4095)]);
    // Right stick to (max, min).
    let r = abses(&run(Side::Right, &[wire(0x00, CENTER, CENTER), wire(0x00, 4095, 0)]));
    assert_eq!(r, vec![(codes::ABS_RX, 4095), (codes::ABS_RY, 0)]);
}

/// A realistic combined frame — several buttons + a stick deflection at once — decodes all of it
/// in one report, and nothing spurious.
#[test]
fn combined_press_and_move_in_one_frame() {
    // Right side: east/right face button (0x10) + R1 (0x01) held, right stick pushed.
    let evs = run(Side::Right, &[wire(0x00, CENTER, CENTER), wire(0x11, 3000, 1000)]);
    let mut k = keys(&evs);
    k.sort();
    let mut want = vec![(codes::BTN_TR, 1), (codes::BTN_EAST, 1)];
    want.sort();
    assert_eq!(k, want, "east + R1 both press in one report");
    assert_eq!(abses(&evs), vec![(codes::ABS_RX, 3000), (codes::ABS_RY, 1000)]);
}

/// The scanner survives real-world stream noise (a garbage prefix + a torn frame) and still
/// yields the intended events.
#[test]
fn decodes_through_stream_noise() {
    let mut scanner = FrameScanner::new();
    let mut decoder = SideDecoder::new(Side::Right);
    // Junk, a false 0xFF, then a clean "south pressed" frame split across two reads.
    let clean = wire(0x20, CENTER, CENTER);
    let mut stream = vec![0x00, 0xAB, 0xFF, 0x77];
    stream.extend_from_slice(&clean[..4]);
    let mut evs = Vec::new();
    for f in scanner.push(&stream) {
        evs = decoder.apply(f);
    }
    for f in scanner.push(&clean[4..]) {
        evs = decoder.apply(f);
    }
    assert_eq!(keys(&evs), vec![(codes::BTN_SOUTH, 1)], "south decoded through noise + a torn frame");
}
