//! REPRODUCTION (tsp-bwrg.6 live-gate finding): the a133 decoder streams the sticks CONTINUOUSLY
//! at a non-zero rest value (~2098), not push-on-change. The guided-collection engine was built +
//! tested against a QUIESCENT-rest model (the a133_synthetic.rs source uses discrete batches with
//! empty separators). This test feeds a REALISTIC continuous stream to expose the mismatch:
//!   - `distinct_abs_codes` filters `value != 0`, so the resting sticks (~2098) always count as
//!     "active" — which makes the DPAD (hat) collection pick the stick axes (ABS_X,ABS_Y) instead
//!     of ABS_HAT0X,ABS_HAT0Y.
//!
//! It documents the CURRENT (broken) behaviour so the fix has a regression target.

use pf_input_collect::collect::{self, DeviceMeta, Recorded, RunConfig};
use pf_input_collect::plan::{ControlSpec, Kind};
use pf_input_collect::source::{AbsInfo, Identity, RawEvent, ScriptedSource};
use pf_input_collect::Collector;

// FRAME VOCABULARY — CONSUMED, never re-derived (tsp-ozbp.14 / runtime#33). `pf_input_decode::codes`
// is where the two evdev code frames are defined and where the POSITIONAL (Frame C) face-button
// constants live. Importing them is deliberate: a local `const BTN_SOUTH: u16 = 0x130` here would be
// a second independent derivation of the mapping, i.e. a second chance to invert it — the exact bug
// tsp-ozbp.14 fixed. Never spell a face button by its printed glyph: this chassis is
// Nintendo-arranged, so the glyph and the kernel's letter alias agree on west/north and are INVERTED
// on south/east, which makes a glyph-keyed fixture look correct exactly half the time.
use pf_input_decode::codes::{BTN_EAST, BTN_SOUTH};

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const STICK: AbsInfo = AbsInfo { min: 0, max: 4095, fuzz: 0, flat: 0, resolution: 0 };
const HAT: AbsInfo = AbsInfo { min: -1, max: 1, fuzz: 0, flat: 0, resolution: 0 };

fn abs(code: u16, val: i32) -> RawEvent { RawEvent::new(EV_ABS, code, val) }

/// One "frame" of the a133's continuous rest stream: all four stick axes at their (non-zero) rest.
fn rest_frame() -> Vec<RawEvent> {
    vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)]
}

fn dut() -> ScriptedSource {
    let ident = Identity { name: "TRIMUI Player1".into(), bus: 3, vid: 0x045e, pid: 0x028e, version: 0x0110 };
    ScriptedSource::new(ident)
        .with_abs(ABS_X, STICK).with_abs(ABS_Y, STICK)
        .with_abs(ABS_RX, STICK).with_abs(ABS_RY, STICK)
        .with_abs(ABS_HAT0X, HAT).with_abs(ABS_HAT0Y, HAT)
}

/// The dpad prompt: continuous rest-stick stream with a real HAT actuation mixed in — exactly what
/// the decoder emits when the owner presses the dpad (the sticks keep streaming at rest).
#[test]
fn dpad_capture_under_continuous_stick_stream() {
    let mut src = dut();
    // Several rest frames, then the dpad press (HAT deflect+return) still amid rest-stick frames.
    for _ in 0..3 { src.push_batch(rest_frame()); }
    let mut press = rest_frame(); press.extend([abs(ABS_HAT0X, -1), abs(ABS_HAT0X, 0), abs(ABS_HAT0Y, 1), abs(ABS_HAT0Y, 0)]);
    src.push_batch(press);
    for _ in 0..3 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "dpad".into(), kind: Kind::Hat, prompt: "dpad".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig {
        quiet_polls: 2,
        idle_skip_polls: 2,
        max_polls: 2000,
        control_timeout: std::time::Duration::from_secs(5),
        ..RunConfig::default()
    };
    let mut log = Vec::new();
    let caps = collect::run(&mut c, &mut src, &meta(), &cfg, &mut log);
    match caps {
        Ok(cap) => {
            let dpad = cap.inputs.iter().find(|i| i.id == "dpad").expect("dpad row");
            println!("dpad collected as: ev_type={} code={}", dpad.ev_type, dpad.code);
            // THE ASSERTION THE FIX MUST SATISFY: the dpad must be the HAT axes, not the sticks.
            assert_eq!(dpad.code, "ABS_HAT0X,ABS_HAT0Y",
                "dpad mis-attributed to {} — the resting sticks (~2098, value!=0) were counted as active",
                dpad.code);
        }
        Err(e) => panic!("dpad collection errored: {e}"),
    }
}

/// FAITHFUL race-bug regression (tsp-bwrg.6, coordinator hard-bar): a REQUIRED control fed ONLY the
/// a133's continuous at-rest stick stream — NO real actuation — must NOT be collected. The engine
/// must return `NoActivity` (the wizard SITS on the control / aborts, rather than fabricating a
/// value), NOT an `Ok(..)` map synthesized from ambient noise.
///
/// This targets the EXACT defect that raced ~14 controls in ~7s and wrote a fabricated map TWICE
/// (two owner trips): the OLD `value != 0` active-test counted the ~2098 rest as actuation, so
/// `finalize` recorded the continuously-streaming axes for a control the owner never touched. The
/// prior test in this file only exercises ATTRIBUTION (rest stream + a REAL hat press); it never
/// covered the no-actuation FABRICATION, which is why the bug shipped under a green suite.
///
/// Fail/pass directions (demonstrated in PR #32): against the OLD (`value != 0`) logic this FAILS
/// (`Ok` with a fabricated stick); against the significance-based fix it PASSES (`NoActivity`).
#[test]
fn required_control_is_not_fabricated_from_the_ambient_rest_stream() {
    let mut src = dut();
    // A long continuous at-rest stream, then quiet — the owner NEVER actuated the stick.
    for _ in 0..500 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig {
        quiet_polls: 2,
        idle_skip_polls: 2,
        max_polls: 600, // runaway guard hit fast (ScriptedSource poll is instant); the point is finalize
        control_timeout: std::time::Duration::from_secs(5),
        ..RunConfig::default()
    };
    let mut log = Vec::new();
    match collect::run(&mut c, &mut src, &meta(), &cfg, &mut log) {
        Err(collect::CollectError::NoActivity { id }) => assert_eq!(id, "lstick"),
        Ok(cap) => panic!(
            "FABRICATED a control from the ambient rest stream (the race bug): {:?}",
            cap.inputs.iter().map(|i| (i.id.clone(), i.code.clone())).collect::<Vec<_>>()
        ),
        Err(e) => panic!("unexpected error (expected NoActivity for an untouched required control): {e}"),
    }
}

/// CRASH-SAFETY + full-sweep capture (tsp-bwrg.6, tsp-e1b-coord's signed16 worry): a REAL circular
/// sweep of BOTH left-stick axes across the full UNSIGNED 0..4095 range — amid the continuous rest
/// stream — must be captured as a `Stick` on `ABS_X,ABS_Y`, and must NOT panic. The collection
/// engine reads the REAL `AbsInfo` (0..4095) via EVIOCGABS and does all activity/observe math in
/// i64 with `.max(1)`-guarded divisors, so there is no signed16 (-32768..32767) normalization to
/// overflow or divide-by-zero on — the "left-thumbstick crash" was in fact an *abort* on a partial
/// (one-axis) sweep, not a range panic. This test locks BOTH facts: a full both-axis sweep yields
/// `Ok(Stick)`, and the run reaching this assertion at all proves no panic on the unsigned range.
#[test]
fn full_both_axis_stick_sweep_captures_on_unsigned_range_without_panicking() {
    let mut src = dut();
    for _ in 0..3 { src.push_batch(rest_frame()); }
    // A circular sweep: both ABS_X and ABS_Y travel edge-to-edge across the full 0..4095 range
    // (the extremes a real "roll it all the way around" produces), still amid the rest stream.
    for &(x, y) in &[(4095, 2048), (2048, 4095), (0, 2048), (2048, 0), (4095, 4095), (0, 0), (2048, 2048)] {
        let mut f = rest_frame();
        f.extend([abs(ABS_X, x), abs(ABS_Y, y)]);
        src.push_batch(f);
    }
    for _ in 0..3 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig {
        quiet_polls: 2,
        idle_skip_polls: 2,
        max_polls: 600,
        control_timeout: std::time::Duration::from_secs(5),
        ..RunConfig::default()
    };
    let mut log = Vec::new();
    match collect::run(&mut c, &mut src, &meta(), &cfg, &mut log) {
        Ok(cap) => {
            let ls = cap.inputs.iter().find(|i| i.id == "lstick").expect("lstick row");
            assert_eq!(ls.code, "ABS_X,ABS_Y", "a full both-axis sweep must record both stick axes, got {}", ls.code);
        }
        Err(e) => panic!("a full both-axis stick sweep must capture cleanly, not error: {e}"),
    }
}

/// SEQUENTIAL two-axis STICK completion (tsp-bwrg.6 — this is the owner's lstick ABORT, same root
/// cause as the dpad wedge). A human roll is not a perfect simultaneous circle: a mid-roll pause,
/// or moving one axis then the other, puts a GAP between the axes. The window must hold open until
/// BOTH axes are seen. Before the fix the gap closed the window on one axis -> finalize needs two
/// -> Incomplete -> the run ABORTED (exactly the owner's "incomplete capture for lstick"). Fed here
/// as X-sweep, GAP, Y-sweep, settle. Fail-old (Err Incomplete) / pass-new (Ok, both axes).
#[test]
fn stick_completes_on_sequential_two_axis_motion() {
    let mut src = dut();
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    // sweep ABS_X only, return to rest
    src.push_batch(lx(4095)); src.push_batch(lx(0)); src.push_batch(lx(2098));
    // a real mid-roll PAUSE (closes a one-axis window under the old logic)
    for _ in 0..3 { src.push_batch(rest_frame()); }
    // then sweep ABS_Y only
    src.push_batch(ly(4095)); src.push_batch(ly(0)); src.push_batch(ly(2107));
    for _ in 0..3 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 600, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    match collect::run(&mut c, &mut src, &meta(), &cfg, &mut log) {
        Ok(cap) => {
            let ls = cap.inputs.iter().find(|i| i.id == "lstick").expect("lstick row");
            assert_eq!(ls.code, "ABS_X,ABS_Y",
                "a sequential two-axis stick motion must record BOTH axes, got {}", ls.code);
        }
        Err(e) => panic!("stick must COMPLETE on a sequential two-axis motion, not abort \
            (this IS the owner's lstick abort): {e}"),
    }
}

/// CALIBRATION (tsp-bwrg.6, owner-directed): the stick row records the OBSERVED per-axis envelope —
/// measured min/max TRAVEL plus the rest/centre (`value`) — not the driver-declared 0..4095. A
/// realistic capture sits mostly at rest (~2098/~2107) with a full sweep to the extremes, so the
/// median of each axis's samples is its resting value. The recorded centre corroborates
/// tsp-ozbp.9's L(~2097,~2107) rest values — the concrete calibration validation the owner asked for.
#[test]
fn stick_records_observed_calibration_envelope() {
    let mut src = dut();
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2097), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    for _ in 0..6 { src.push_batch(rest_frame()); }
    src.push_batch(lx(12)); src.push_batch(lx(4083));    // X sweep to observed extremes
    for _ in 0..6 { src.push_batch(rest_frame()); }
    src.push_batch(ly(9)); src.push_batch(ly(4090));      // Y sweep to observed extremes
    for _ in 0..6 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 600, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    let cap = collect::run(&mut c, &mut src, &meta(), &cfg, &mut log).expect("lstick must complete");
    let ls = cap.inputs.iter().find(|i| i.id == "lstick").expect("lstick row");
    let x = ls.x.expect("lstick x axis");
    let y = ls.y.expect("lstick y axis");
    // Observed TRAVEL, not the declared 0..4095 full-scale.
    assert_eq!((x.min, x.max), (12, 4083), "x records OBSERVED travel, not declared range");
    assert_eq!((y.min, y.max), (9, 4090), "y records OBSERVED travel, not declared range");
    // Rest/centre recorded and corroborating tsp-ozbp.9's rest values.
    assert!((x.value.expect("x centre recorded") - 2098).abs() <= 3, "x centre ~ rest, got {:?}", x.value);
    assert!((y.value.expect("y centre recorded") - 2107).abs() <= 3, "y centre ~ rest, got {:?}", y.value);
}

/// tsp-bwrg.6 OWNER PASS #5: the stick advanced after ~0.25s (both axes merely TOUCHED) on a brief
/// mid-roll centre-transit, truncating the roll and cascading its remainder into later controls.
/// The full-sweep completion gate keeps the window open until BOTH axes reach their extremes, so a
/// mid-roll pause does NOT complete it and the WHOLE min/max envelope is captured. Fail-old
/// (completes at the first corner, x.min ~2097) / pass-new (full 0..4095 envelope).
#[test]
fn stick_full_roll_survives_a_mid_roll_pause_and_records_the_whole_envelope() {
    let mut src = dut();
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2097), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    // First corner: X then Y to MAX (both axes seen active) ...
    src.push_batch(lx(4095)); src.push_batch(ly(4095));
    // ... then a real mid-roll PAUSE at rest (would complete under the old "both axes seen" gate) ...
    for _ in 0..4 { src.push_batch(rest_frame()); }
    // ... then the REST of the roll: both axes to MIN, then settle at rest.
    src.push_batch(lx(0)); src.push_batch(ly(0));
    for _ in 0..4 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let spec = ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false };
    let mut c = Collector::new(vec![spec]);
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 600, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    let cap = collect::run(&mut c, &mut src, &meta(), &cfg, &mut log).expect("lstick completes after the full roll");
    let ls = cap.inputs.iter().find(|i| i.id == "lstick").expect("lstick row");
    let x = ls.x.expect("x axis");
    let y = ls.y.expect("y axis");
    // The WHOLE envelope was captured — the mid-roll pause did NOT truncate the roll at the corner.
    assert!(x.min <= 100 && x.max >= 3995, "x must span the full roll (mid-roll pause must not truncate), got {}..{}", x.min, x.max);
    assert!(y.min <= 100 && y.max >= 3995, "y must span the full roll, got {}..{}", y.min, y.max);
}

/// tsp-bwrg.6 OWNER PASS #5, the CASCADE half: the stick completed early and the REST of the roll
/// leaked into the NEXT control and falsely satisfied it (the following stick captured the FIRST
/// stick's axes — "wrongly associated the rest of the roll with other buttons"). The full-sweep gate
/// keeps the roll inside its own control until the roll ENDS, so the next control gets ITS OWN
/// input. Fail-old (rstick captures ABS_X,ABS_Y from the bleed) / pass-new (rstick=ABS_RX,ABS_RY).
#[test]
fn a_long_stick_roll_does_not_bleed_into_the_next_control() {
    let mut src = dut();
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2097), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let rx = |x: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, x), abs(ABS_RY, 2129)];
    let ry = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, y)];
    // lstick: one corner, a mid-roll PAUSE (the old early-complete point), then finish to the
    // opposite extremes, then MORE roll (the "excess" that used to bleed) — all before any settle.
    src.push_batch(lx(4095)); src.push_batch(ly(4095));
    for _ in 0..3 { src.push_batch(rest_frame()); }
    src.push_batch(lx(0)); src.push_batch(ly(0));
    src.push_batch(lx(4095)); src.push_batch(ly(0)); src.push_batch(lx(0)); src.push_batch(ly(4095));
    // A real human INTER-CONTROL gap (the owner lets go, reads the next prompt, reaches over). It
    // must outlast BOTH the settle that closes lstick's window AND the inter-control drain that
    // follows it (tsp-bwrg.12) — a 3-frame gap was a machine-timed fixture, not a human one.
    for _ in 0..12 { src.push_batch(rest_frame()); }
    // rstick: its OWN roll on RX/RY.
    src.push_batch(rx(4095)); src.push_batch(rx(0)); src.push_batch(ry(4095)); src.push_batch(ry(0));
    for _ in 0..3 { src.push_batch(rest_frame()); }
    src.push_batch(vec![]); src.push_batch(vec![]);

    let plan = vec![
        ControlSpec { id: "lstick".into(), kind: Kind::Stick, prompt: "lstick".into(), optional: false },
        ControlSpec { id: "rstick".into(), kind: Kind::Stick, prompt: "rstick".into(), optional: false },
    ];
    let mut c = Collector::new(plan);
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 2000, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    let cap = collect::run(&mut c, &mut src, &meta(), &cfg, &mut log).expect("both sticks complete");
    let ls = cap.inputs.iter().find(|i| i.id == "lstick").expect("lstick row");
    let rs = cap.inputs.iter().find(|i| i.id == "rstick").expect("rstick row");
    assert_eq!(ls.code, "ABS_X,ABS_Y", "lstick must own the left-stick axes");
    assert_eq!(rs.code, "ABS_RX,ABS_RY",
        "rstick must capture ITS OWN axes — the lstick roll must not bleed in and falsely satisfy it");
}

/// The FOUR atomic dpad direction steps (HatDir) each complete on their own single-axis press —
/// removing the dpad from the 2-axis-single-window class entirely (owner-directed, tsp-bwrg.6) —
/// and MERGE at emit into ONE hat row (`ABS_HAT0X,ABS_HAT0Y`), so the collected map is unchanged
/// from a single hat control. Fed with realistic gaps between directions; no per-direction row leaks.
#[test]
fn four_dpad_direction_steps_merge_to_one_hat_row() {
    let mut src = dut();
    let dir = |code: u16, v: i32| { let mut f = rest_frame(); f.extend([abs(code, v), abs(code, 0)]); f };
    // Human INTER-CONTROL gaps: long enough to outlast the settle AND the inter-control drain
    // (tsp-bwrg.12). A 2-frame gap models a machine, not a person moving between directions.
    src.push_batch(dir(ABS_HAT0Y, -1)); for _ in 0..12 { src.push_batch(rest_frame()); } // up
    src.push_batch(dir(ABS_HAT0Y, 1));  for _ in 0..12 { src.push_batch(rest_frame()); } // down
    src.push_batch(dir(ABS_HAT0X, -1)); for _ in 0..12 { src.push_batch(rest_frame()); } // left
    src.push_batch(dir(ABS_HAT0X, 1));  for _ in 0..12 { src.push_batch(rest_frame()); } // right
    src.push_batch(vec![]); src.push_batch(vec![]);

    let plan = vec![
        ControlSpec { id: "dpad_up".into(), kind: Kind::HatDir, prompt: "up".into(), optional: false },
        ControlSpec { id: "dpad_down".into(), kind: Kind::HatDir, prompt: "down".into(), optional: false },
        ControlSpec { id: "dpad_left".into(), kind: Kind::HatDir, prompt: "left".into(), optional: false },
        ControlSpec { id: "dpad_right".into(), kind: Kind::HatDir, prompt: "right".into(), optional: false },
    ];
    let mut c = Collector::new(plan);
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 600, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    match collect::run(&mut c, &mut src, &meta(), &cfg, &mut log) {
        Ok(cap) => {
            let ids: Vec<&str> = cap.inputs.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["dpad"], "the four dpad steps must merge to exactly ONE dpad row, got {ids:?}");
            assert_eq!(cap.inputs[0].code, "ABS_HAT0X,ABS_HAT0Y");
        }
        Err(e) => panic!("four dpad direction steps must each complete + merge, not error: {e}"),
    }
}

/// The corrected owner-free gate (tsp-e1b-coord's standard): synthesize evdev for the FULL
/// 17-PROMPT a133 plan — 9 buttons, the FOUR atomic dpad direction steps, both sticks (SEQUENTIAL
/// two-axis with a gap), the two binary triggers — amid the continuous rest-stick stream, and
/// assert the run walks the WHOLE plan to the END and emits the 14-control collected map (dpad
/// merged to one hat row).
///
/// NAMED FINDING (tsp-bwrg.6, the third sighting tonight): the OLD suite fed MACHINE-timed input —
/// presses with NO inter-press gaps — to a UI designed for HUMAN-timed input, which ALWAYS has
/// gaps. That timing profile never closed a 2-axis window mid-press, so the suite stayed green
/// while the bug shipped. The GAPS here are LOAD-BEARING: between the stick's two axes sits a real
/// pause, which is exactly what closed the window on one axis under the pre-fix "settle on first
/// axis" logic (Err Incomplete -> ABORT). Fail-old / pass-new (seen_axes>=2 + dpad-out-of-class).
#[test]
fn full_17_prompt_plan_walks_to_completion() {
    use pf_input_collect::plan::a133_gamepad_plan;
    use pf_input_decode::codes::{
        BTN_MODE, BTN_NORTH, BTN_SELECT, BTN_START, BTN_TL, BTN_TL2, BTN_TR, BTN_TR2, BTN_WEST,
    };
    let key = |code: u16| { let mut f = rest_frame(); f.extend([RawEvent::new(EV_KEY, code, 1), RawEvent::new(EV_KEY, code, 0)]); f };
    let dir = |code: u16, v: i32| { let mut f = rest_frame(); f.extend([abs(code, v), abs(code, 0)]); f };
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let rx = |x: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, x), abs(ABS_RY, 2129)];
    let ry = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, y)];
    // The 9 button prompts IN PLAN ORDER, each paired with the code that PHYSICAL POSITION emits —
    // taken from `pf_input_decode::codes` (Frame C, positional), never a letter. The prior fixture
    // spelled these as `(a, b, x, y, …)` and, reading the letters positionally, fed 0x133 to the
    // WEST prompt and 0x134 to the NORTH prompt — inverted. It never showed up because the test only
    // asserted that ids were PRESENT, not that each control recorded ITS OWN code. That is the
    // glyph-vs-position trap in miniature (tsp-ozbp.14): west/north agree by coincidence on this
    // chassis, so a letter-keyed fixture looks right exactly half the time.
    let buttons: [(&str, u16); 9] = [
        ("south", BTN_SOUTH),
        ("east", BTN_EAST),
        ("west", BTN_WEST),
        ("north", BTN_NORTH),
        ("select", BTN_SELECT),
        ("start", BTN_START),
        ("guide", BTN_MODE),
        ("l1", BTN_TL),
        ("r1", BTN_TR),
    ];
    // A human INTER-CONTROL gap. It must outlast BOTH the settle that closes a control's window AND
    // the inter-control drain that follows it (tsp-bwrg.12). The old 2-3 frame gaps were
    // MACHINE-timed — the exact fixture profile that kept this suite green while the bugs shipped.
    let gap = |src: &mut ScriptedSource, n: usize| { for _ in 0..n { src.push_batch(rest_frame()); } };
    const HUMAN_GAP: usize = 12;
    let mut src = dut();
    for (_, code) in buttons { src.push_batch(key(code)); gap(&mut src, HUMAN_GAP); }
    // 4 dpad directions — each an ATOMIC single-axis press, each followed by a gap (up,down,left,right)
    for (code, v) in [(ABS_HAT0Y, -1), (ABS_HAT0Y, 1), (ABS_HAT0X, -1), (ABS_HAT0X, 1)] {
        src.push_batch(dir(code, v)); gap(&mut src, HUMAN_GAP);
    }
    // lstick — SEQUENTIAL X then a real mid-roll PAUSE then Y (the 2-axis case the fix protects)
    src.push_batch(lx(4095)); src.push_batch(lx(0)); gap(&mut src, HUMAN_GAP);
    src.push_batch(ly(4095)); src.push_batch(ly(0)); gap(&mut src, HUMAN_GAP);
    // rstick — SEQUENTIAL RX then gap then RY
    src.push_batch(rx(4095)); src.push_batch(rx(0)); gap(&mut src, HUMAN_GAP);
    src.push_batch(ry(4095)); src.push_batch(ry(0)); gap(&mut src, HUMAN_GAP);
    // ltrig, rtrig — binary buttons (BTN_TL2 / BTN_TR2)
    src.push_batch(key(BTN_TL2)); gap(&mut src, HUMAN_GAP);
    src.push_batch(key(BTN_TR2)); gap(&mut src, HUMAN_GAP);
    src.push_batch(vec![]); src.push_batch(vec![]);

    let mut c = Collector::new(a133_gamepad_plan());
    let cfg = RunConfig { quiet_polls: 2, idle_skip_polls: 40, max_polls: 10000, control_timeout: std::time::Duration::from_secs(5), ..RunConfig::default() };
    let mut log = Vec::new();
    match collect::run(&mut c, &mut src, &meta(), &cfg, &mut log) {
        Ok(cap) => {
            let ids: Vec<&str> = cap.inputs.iter().map(|i| i.id.as_str()).collect();
            // Collected MAP = 14 controls (the four dpad PROMPTS merge to ONE dpad hat CONTROL).
            for want in ["south", "east", "west", "north", "select", "start", "guide", "l1", "r1",
                "dpad", "lstick", "rstick", "ltrig", "rtrig"]
            {
                assert!(ids.contains(&want), "control {want} missing — the walk did not advance through it: {ids:?}");
            }
            // Splitting a PROMPT must NEVER split a CONTROL: exactly one dpad row, both hat axes.
            assert_eq!(ids.iter().filter(|&&i| i == "dpad").count(), 1, "dpad must merge to ONE row: {ids:?}");
            let dpad = cap.inputs.iter().find(|i| i.id == "dpad").unwrap();
            assert_eq!(dpad.code, "ABS_HAT0X,ABS_HAT0Y");
            // Each button control recorded ITS OWN code — a walk that merely reaches the end proves
            // nothing about WHICH control captured what, and a bleed shifts every capture by one
            // while still emitting a full, plausible-looking map (tsp-bwrg.12). Anchored on the
            // position id -> positional code pairs the fixture was built from.
            for (id, code) in buttons {
                assert_eq!(
                    c.recorded(id),
                    Some(&Recorded::Button { code }),
                    "{id} must record the code its own physical position emitted"
                );
            }
        }
        Err(e) => panic!("the full 17-prompt plan must walk to completion, not abort: {e}"),
    }
}

fn meta() -> DeviceMeta { DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() } }

// =============================================================================================
// tsp-bwrg.12 — ADVERSARIAL HUMAN-TIMED TESTS for the BUTTON and DPAD classes.
//
// tsp-bwrg.6 replaced quiet-driven completion for the STICK class (a structural coverage gate:
// both axes must reach both extremes, THEN settle). The button and dpad classes were left with
// two holes this section exercises, one per defect:
//
//   (A) NO INTER-CONTROL DRAIN. `run()` commits control N and opens control N+1's window with no
//       gap, so whatever the device emits in between — a "did that take?" re-press, an overshoot
//       past the prompted direction, a key-up bounce — lands in the NEXT control's buffer and can
//       satisfy it. The next control then records the PREVIOUS control's input and the wizard
//       advances past a control the owner never actuated. This is silent: the emitted map can
//       still look plausible (see `dpad_prompt_rejects_the_previous_directions_overshoot`, where
//       the merged hat row is correct while every direction is shifted by one).
//
//   (B) COMPLETION IS STILL QUIET-DRIVEN FOR THE NON-STICK CLASSES. `axes_fully_swept` returns
//       `true` unconditionally for a 1-axis control, so a HatDir/Trigger window closes on
//       `quiet >= quiet_polls` after ANY activity — and "activity" is any significant abs
//       deviation on ANY axis, or any key-down. A thumb brushing the stick on the way to the
//       D-PAD is therefore enough to complete a dpad direction, and `finalize`'s
//       `.or_else(|| axes.first())` fallback then records the STICK axis as that direction.
//
// TIMING DISCIPLINE (the tsp-bwrg.6 named finding, applied): every fixture below is HUMAN-timed —
// real gaps between controls, real pauses mid-actuation. Machine-timed fixtures (presses with no
// gaps) are what kept the suite green while these bugs shipped; the gaps here are load-bearing.
//
// ASSERTION SURFACE: these assert on `Collector::recorded(id)` — the PER-CONTROL capture — not on
// the emitted rows. The emitted dpad row is the merged SET of hat axes, which stays correct even
// when every direction captured the wrong one; asserting on the emitted map is precisely how a
// test of this defect would pass while the defect shipped.
//
// NEGATIVE CONTROL: each of these FAILS against the pre-change quiet-based logic and PASSES after.
// The per-test pre-change failure is recorded in the PR's test plan.
// =============================================================================================

fn key(code: u16, val: i32) -> RawEvent { RawEvent::new(EV_KEY, code, val) }

/// A press of `code` riding the continuous at-rest stick stream (down + up in one poll, which is
/// what a ~48fps decoder emits for a normal human tap).
fn press_frame(code: u16) -> Vec<RawEvent> {
    let mut f = rest_frame();
    f.extend([key(code, 1), key(code, 0)]);
    f
}

/// One D-PAD direction actuated on `code` (deflect to `v`, return to centre) amid the rest stream.
fn hat_frame(code: u16, v: i32) -> Vec<RawEvent> {
    let mut f = rest_frame();
    f.extend([abs(code, v), abs(code, 0)]);
    f
}

/// A real human INTER-CONTROL gap, in polls: the owner lets go, sees the prompt change, reads it,
/// and reaches for the next control. It must outlast BOTH the settle that closes a control's window
/// AND the fixed inter-control drain that follows it — which is the point, because a gap shorter
/// than the drain is not a human gap at all. Sized well clear of both so these fixtures assert on
/// the POLICY, not on off-by-one poll accounting.
const INTER_CONTROL_GAP: usize = 12;

fn push_rest(src: &mut ScriptedSource, n: usize) {
    for _ in 0..n {
        src.push_batch(rest_frame());
    }
}

/// The human-timed config these tests share: a short settle, and a per-control window bounded only
/// by the runaway guard (`ScriptedSource::poll` never blocks, so wall-clock is not the bound here).
fn human_cfg() -> RunConfig {
    RunConfig {
        quiet_polls: 2,
        idle_skip_polls: 40,
        max_polls: 2000,
        control_timeout: std::time::Duration::from_secs(5),
        ..RunConfig::default()
    }
}

/// DEFECT (A), BUTTON class, INTER-CONTROL GAP. The owner presses the BOTTOM face button, is not
/// sure it registered, and presses it AGAIN — a beat after the window already closed. That second
/// press lands in the gap between controls, and with no drain it is the first key-down in the NEXT
/// control's buffer, so the RIGHT face button silently records the BOTTOM button's code.
///
/// Anchored on POSITION (`BTN_SOUTH`/`BTN_EAST` from `pf_input_decode::codes`), never a glyph: on
/// this Nintendo-arranged chassis the bottom button is printed "B" and the right one "A".
///
/// Fail-pre (east records BTN_SOUTH — the tail) / pass-post (the drain discards the tail; east
/// records its own BTN_EAST).
#[test]
fn button_prompt_rejects_the_previous_controls_key_tail() {
    let mut src = dut();
    src.push_batch(press_frame(BTN_SOUTH)); // the owner's south press — window closes on this poll
    push_rest(&mut src, 2); // a beat: the owner watches to see whether the prompt advanced ...
    src.push_batch(press_frame(BTN_SOUTH)); // ... decides it did not, and presses AGAIN
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(press_frame(BTN_EAST)); // the owner's real east press
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(vec![]);
    src.push_batch(vec![]);

    let plan = vec![
        ControlSpec { id: "south".into(), kind: Kind::Button, prompt: "south".into(), optional: false },
        ControlSpec { id: "east".into(), kind: Kind::Button, prompt: "east".into(), optional: false },
    ];
    let mut c = Collector::new(plan);
    let mut log = Vec::new();
    let run = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log);

    assert_eq!(
        c.recorded("south"),
        Some(&Recorded::Button { code: BTN_SOUTH }),
        "the south prompt must record the south press"
    );
    assert_eq!(
        c.recorded("east"),
        Some(&Recorded::Button { code: BTN_EAST }),
        "the east prompt captured the PRECEDING control's key tail instead of its own press — \
         an inter-control drain must discard input in the gap (run result: {:?})",
        run.as_ref().map(|_| "Ok").map_err(|e| e.to_string())
    );
}

/// DEFECT (A), BUTTON class, OVERSHOOT. Same mechanism, the other human shape: instead of one
/// late re-press the owner OVERSHOOTS the instruction and keeps pressing for several more polls
/// after the window closed. The drain must absorb a multi-poll tail, not just a single stray poll.
///
/// Fail-pre (east records BTN_SOUTH from the first overshoot poll) / pass-post (east records
/// BTN_EAST).
#[test]
fn button_prompt_rejects_a_multi_poll_overshoot_from_the_previous_control() {
    let mut src = dut();
    src.push_batch(press_frame(BTN_SOUTH)); // window closes here
    push_rest(&mut src, 1);
    for _ in 0..3 {
        src.push_batch(press_frame(BTN_SOUTH)); // overshoot: three more presses past the close
    }
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(press_frame(BTN_EAST));
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(vec![]);
    src.push_batch(vec![]);

    let plan = vec![
        ControlSpec { id: "south".into(), kind: Kind::Button, prompt: "south".into(), optional: false },
        ControlSpec { id: "east".into(), kind: Kind::Button, prompt: "east".into(), optional: false },
    ];
    let mut c = Collector::new(plan);
    let mut log = Vec::new();
    let _ = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log);

    assert_eq!(
        c.recorded("east"),
        Some(&Recorded::Button { code: BTN_EAST }),
        "a multi-poll overshoot of the PREVIOUS control satisfied the east prompt — the drain must \
         absorb the whole tail, not one poll of it"
    );
}

/// DEFECT (A), DPAD class, OVERSHOOT — and the case that shows why this must be asserted
/// PER-CONTROL. The owner presses UP and, rocking the D-PAD, overshoots onto LEFT a beat after the
/// UP window closed. With no drain that overshoot satisfies the DOWN prompt, the owner's real DOWN
/// press then satisfies the LEFT prompt, and so on: every direction is shifted by one and the
/// wizard advances past controls the owner never actuated for.
///
/// The EMITTED map still looks right (the four captures merge into the set {HAT0X, HAT0Y}), which
/// is exactly how a map-level test of this defect passes while the defect ships. Assert the
/// per-direction capture instead: a vertical direction must record the vertical hat axis.
///
/// Fail-pre (dpad_down records ABS_HAT0X — the LEFT overshoot) / pass-post (each direction records
/// its own axis).
#[test]
fn dpad_prompt_rejects_the_previous_directions_overshoot() {
    let mut src = dut();
    src.push_batch(hat_frame(ABS_HAT0Y, -1)); // UP — window closes 2 quiet polls later
    push_rest(&mut src, 2);
    src.push_batch(hat_frame(ABS_HAT0X, -1)); // OVERSHOOT onto LEFT, just past the close
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(hat_frame(ABS_HAT0Y, 1)); // DOWN
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(hat_frame(ABS_HAT0X, -1)); // LEFT
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(hat_frame(ABS_HAT0X, 1)); // RIGHT
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(vec![]);
    src.push_batch(vec![]);

    let plan = ["dpad_up", "dpad_down", "dpad_left", "dpad_right"]
        .into_iter()
        .map(|id| ControlSpec { id: id.into(), kind: Kind::HatDir, prompt: id.into(), optional: false })
        .collect();
    let mut c = Collector::new(plan);
    let mut log = Vec::new();
    let _ = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log);

    assert_eq!(
        c.recorded("dpad_up"),
        Some(&Recorded::HatAxis { code: ABS_HAT0Y }),
        "UP must record the VERTICAL hat axis"
    );
    assert_eq!(
        c.recorded("dpad_down"),
        Some(&Recorded::HatAxis { code: ABS_HAT0Y }),
        "DOWN recorded the horizontal axis — the UP overshoot bled across the control boundary and \
         satisfied this prompt, so the owner's real DOWN press was never what completed it"
    );
    assert_eq!(
        c.recorded("dpad_left"),
        Some(&Recorded::HatAxis { code: ABS_HAT0X }),
        "LEFT must record the HORIZONTAL hat axis (it captured the cascaded DOWN press instead)"
    );
    assert_eq!(
        c.recorded("dpad_right"),
        Some(&Recorded::HatAxis { code: ABS_HAT0X }),
        "RIGHT must record the HORIZONTAL hat axis"
    );
}

/// DEFECT (B), DPAD class, MID-ACTUATION PAUSE. The owner's thumb brushes the LEFT STICK on the way
/// across to the D-PAD (the two sit side by side on this chassis), then they PAUSE while finding
/// the direction, and only then press UP. The brush is a significant deflection, so it counts as
/// "activity"; the pause is then enough quiet to close a 1-axis window; and `finalize`'s
/// no-hat-axis fallback records ABS_X as the dpad direction. Nothing errors — the wizard advances
/// and the collected map names a STICK axis as part of the D-PAD.
///
/// This is pure completion-policy: no drain is involved (the crosstalk is inside the control's own
/// window). Completion for a dpad direction must be HAT-ACTUATION-gated, not quiet-gated.
///
/// Fail-pre (dpad_up records ABS_X) / pass-post (the window stays open through the pause and
/// records ABS_HAT0Y from the real press).
#[test]
fn dpad_prompt_survives_a_mid_actuation_pause_with_stick_crosstalk() {
    let mut src = dut();
    // Thumb brushes the left stick reaching across for the D-PAD.
    let mut brush = rest_frame();
    brush.extend([abs(ABS_X, 4095), abs(ABS_X, 2098)]);
    src.push_batch(brush);
    push_rest(&mut src, 5); // the owner PAUSES, hunting for the direction
    src.push_batch(hat_frame(ABS_HAT0Y, -1)); // the real UP press
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(hat_frame(ABS_HAT0X, -1)); // the real LEFT press
    push_rest(&mut src, INTER_CONTROL_GAP);
    src.push_batch(vec![]);
    src.push_batch(vec![]);

    let plan = ["dpad_up", "dpad_left"]
        .into_iter()
        .map(|id| ControlSpec { id: id.into(), kind: Kind::HatDir, prompt: id.into(), optional: false })
        .collect();
    let mut c = Collector::new(plan);
    let mut log = Vec::new();
    let run = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log);

    assert_eq!(
        c.recorded("dpad_up"),
        Some(&Recorded::HatAxis { code: ABS_HAT0Y }),
        "the UP prompt completed on STICK crosstalk plus a human pause and recorded a stick axis as \
         a D-PAD direction — a dpad direction must complete on a HAT actuation, never on quiet"
    );
    assert_eq!(
        c.recorded("dpad_left"),
        Some(&Recorded::HatAxis { code: ABS_HAT0X }),
        "the LEFT prompt must record the horizontal hat axis"
    );
    let cap = run.expect("both directions must capture and merge into one hat row");
    let dpad = cap.inputs.iter().find(|i| i.id == "dpad").expect("merged dpad row");
    assert_eq!(
        dpad.code, "ABS_HAT0X,ABS_HAT0Y",
        "the merged D-PAD row must be the two HAT axes, not a stick axis"
    );
}

/// DEFECT (B), TRIGGER class, MID-ACTUATION PAUSE. An ANALOG trigger emits no key-down, so its only
/// completion path is the 1-axis quiet break — which fires on a partial squeeze. The owner squeezes
/// most of the way, pauses (a normal thing when told "squeeze it fully"), then finishes the press.
/// The pause closes the window at partial travel, `finalize` rules the trigger never reached a full
/// press, and the whole run ABORTS on `Incomplete`.
///
/// A trigger must complete on COVERAGE — a key-down (the a133's binary L2/R2 button-triggers) or an
/// axis that actually reached a full press — never on quiet.
///
/// Fail-pre (`Err(Incomplete)`: "trigger never reached a full press") / pass-post (Ok, analog).
#[test]
fn analog_trigger_survives_a_mid_squeeze_pause() {
    const ABS_Z: u16 = 0x02;
    const TRIG: AbsInfo = AbsInfo { min: 0, max: 255, fuzz: 0, flat: 0, resolution: 0 };
    let ident = Identity { name: "Generic Pad".into(), bus: 3, vid: 0x1234, pid: 0x5678, version: 1 };
    let mut src = ScriptedSource::new(ident).with_abs(ABS_Z, TRIG);
    // Squeeze most of the way — past the activity threshold, short of a full press ...
    src.push_batch([0, 40, 90, 140, 200].iter().map(|&v| abs(ABS_Z, v)).collect());
    for _ in 0..5 {
        src.push_batch(vec![]); // ... then PAUSE mid-squeeze ...
    }
    src.push_batch([210, 230, 255].iter().map(|&v| abs(ABS_Z, v)).collect()); // ... then finish it.
    src.push_batch(vec![]);
    src.push_batch(vec![]);

    let plan = vec![ControlSpec { id: "ltrig".into(), kind: Kind::Trigger, prompt: "ltrig".into(), optional: false }];
    let mut c = Collector::new(plan);
    let mut log = Vec::new();
    let cap = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log)
        .expect("a mid-squeeze PAUSE must not complete the trigger at partial travel and abort the run");
    let t = cap.inputs.iter().find(|i| i.id == "ltrig").expect("ltrig row");
    assert_eq!(t.code, "ABS_Z");
    assert_eq!(
        t.semantics.as_deref(),
        Some("analog"),
        "the full squeeze (with its intermediate travel) must be inside the capture window"
    );
}

/// THE GENERIC INVARIANT, exercised end-to-end: FOREIGN actuation — real input that is not this
/// control's own kind of actuation — must NEVER complete a control, for ANY class. Quiet is not a
/// completion signal, and neither is "something moved".
///
/// One row per class, each fed a stream that is busy but carries nothing the control could
/// legitimately be. A control that cannot be honestly captured must report `NoActivity` (the
/// wizard sits on it / re-prompts), never a capture synthesized from whatever else was moving —
/// the same "never fabricate" bar `required_control_is_not_fabricated_from_the_ambient_rest_stream`
/// set for the ambient case.
///
/// Fail-pre (the HatDir row completes on a stick sweep and records ABS_X) / pass-post (NoActivity).
#[test]
fn foreign_actuation_never_completes_a_control() {
    // A full left-stick sweep — loud, unambiguous, real input, and not a button, hat, or trigger.
    let stick_sweep = |src: &mut ScriptedSource| {
        for &v in &[4095, 2098, 0, 2098] {
            let mut f = rest_frame();
            f.extend([abs(ABS_X, v), abs(ABS_Y, v)]);
            src.push_batch(f);
        }
        push_rest(src, INTER_CONTROL_GAP);
        src.push_batch(vec![]);
        src.push_batch(vec![]);
    };

    for (id, kind) in [
        ("south", Kind::Button),
        ("l3", Kind::StickClick),
        ("dpad_up", Kind::HatDir),
    ] {
        let mut src = dut();
        stick_sweep(&mut src);
        let plan = vec![ControlSpec { id: id.into(), kind, prompt: id.into(), optional: false }];
        let mut c = Collector::new(plan);
        let mut log = Vec::new();
        let got = collect::run(&mut c, &mut src, &meta(), &human_cfg(), &mut log);
        // The capture itself is the evidence: a control completed by foreign input has a `recorded`
        // value naming whatever else happened to be moving. Assert on that FIRST — the run's own
        // Result can fail later for a downstream reason (a one-axis dpad merge, say) and mask the
        // fact that the control was captured at all.
        assert_eq!(
            c.recorded(id),
            None,
            "{id} ({kind:?}) was CAPTURED from a LEFT-STICK SWEEP — foreign actuation must never \
             complete a control of another class"
        );
        match got {
            Err(collect::CollectError::NoActivity { id: got_id }) => assert_eq!(got_id, id),
            other => panic!(
                "{id} ({kind:?}) must report NoActivity when only foreign input was seen; got {:?}",
                other.map(|c| c.inputs.iter().map(|i| (i.id.clone(), i.code.clone())).collect::<Vec<_>>())
                     .map_err(|e| e.to_string())
            ),
        }
    }
}
