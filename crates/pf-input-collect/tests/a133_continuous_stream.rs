//! REPRODUCTION (tsp-bwrg.6 live-gate finding): the a133 decoder streams the sticks CONTINUOUSLY
//! at a non-zero rest value (~2098), not push-on-change. The guided-collection engine was built +
//! tested against a QUIESCENT-rest model (the a133_synthetic.rs source uses discrete batches with
//! empty separators). This test feeds a REALISTIC continuous stream to expose the mismatch:
//!   - `distinct_abs_codes` filters `value != 0`, so the resting sticks (~2098) always count as
//!     "active" — which makes the DPAD (hat) collection pick the stick axes (ABS_X,ABS_Y) instead
//!     of ABS_HAT0X,ABS_HAT0Y.
//! It documents the CURRENT (broken) behaviour so the fix has a regression target.

use pf_input_collect::collect::{self, DeviceMeta, RunConfig};
use pf_input_collect::plan::{ControlSpec, Kind};
use pf_input_collect::source::{AbsInfo, Identity, RawEvent, ScriptedSource};
use pf_input_collect::Collector;

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

/// The FOUR atomic dpad direction steps (HatDir) each complete on their own single-axis press —
/// removing the dpad from the 2-axis-single-window class entirely (owner-directed, tsp-bwrg.6) —
/// and MERGE at emit into ONE hat row (`ABS_HAT0X,ABS_HAT0Y`), so the collected map is unchanged
/// from a single hat control. Fed with realistic gaps between directions; no per-direction row leaks.
#[test]
fn four_dpad_direction_steps_merge_to_one_hat_row() {
    let mut src = dut();
    let dir = |code: u16, v: i32| { let mut f = rest_frame(); f.extend([abs(code, v), abs(code, 0)]); f };
    src.push_batch(dir(ABS_HAT0Y, -1)); for _ in 0..2 { src.push_batch(rest_frame()); } // up
    src.push_batch(dir(ABS_HAT0Y, 1));  for _ in 0..2 { src.push_batch(rest_frame()); } // down
    src.push_batch(dir(ABS_HAT0X, -1)); for _ in 0..2 { src.push_batch(rest_frame()); } // left
    src.push_batch(dir(ABS_HAT0X, 1));  for _ in 0..2 { src.push_batch(rest_frame()); } // right
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
    const EV_KEY: u16 = 0x01;
    let key = |code: u16| { let mut f = rest_frame(); f.extend([RawEvent::new(EV_KEY, code, 1), RawEvent::new(EV_KEY, code, 0)]); f };
    let dir = |code: u16, v: i32| { let mut f = rest_frame(); f.extend([abs(code, v), abs(code, 0)]); f };
    let lx = |x: i32| vec![abs(ABS_X, x), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let ly = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, y), abs(ABS_RX, 2013), abs(ABS_RY, 2129)];
    let rx = |x: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, x), abs(ABS_RY, 2129)];
    let ry = |y: i32| vec![abs(ABS_X, 2098), abs(ABS_Y, 2107), abs(ABS_RX, 2013), abs(ABS_RY, y)];
    // BTN codes: A,B,X,Y, TL(L1),TR(R1), TL2(L2),TR2(R2), SELECT,START, MODE(Menu)
    let (a, b, x, y, tl, tr, tl2, tr2, sel, start, mode) =
        (0x130u16, 0x131, 0x133, 0x134, 0x136, 0x137, 0x138, 0x139, 0x13a, 0x13b, 0x13c);
    let gap = |src: &mut ScriptedSource, n: usize| { for _ in 0..n { src.push_batch(rest_frame()); } };
    let mut src = dut();
    // 9 buttons in plan order — each with a trailing human GAP: south,east,west,north,select,start,guide,l1,r1
    for code in [a, b, x, y, sel, start, mode, tl, tr] { src.push_batch(key(code)); gap(&mut src, 2); }
    // 4 dpad directions — each an ATOMIC single-axis press, each followed by a gap (up,down,left,right)
    for (code, v) in [(ABS_HAT0Y, -1), (ABS_HAT0Y, 1), (ABS_HAT0X, -1), (ABS_HAT0X, 1)] {
        src.push_batch(dir(code, v)); gap(&mut src, 2);
    }
    // lstick — SEQUENTIAL X then a real mid-roll PAUSE then Y (the 2-axis case the fix protects)
    src.push_batch(lx(4095)); src.push_batch(lx(0)); gap(&mut src, 3);
    src.push_batch(ly(4095)); src.push_batch(ly(0)); gap(&mut src, 3);
    // rstick — SEQUENTIAL RX then gap then RY
    src.push_batch(rx(4095)); src.push_batch(rx(0)); gap(&mut src, 3);
    src.push_batch(ry(4095)); src.push_batch(ry(0)); gap(&mut src, 3);
    // ltrig, rtrig — binary buttons (BTN_TL2 / BTN_TR2)
    src.push_batch(key(tl2)); gap(&mut src, 2);
    src.push_batch(key(tr2)); gap(&mut src, 2);
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
        }
        Err(e) => panic!("the full 17-prompt plan must walk to completion, not abort: {e}"),
    }
}

fn meta() -> DeviceMeta { DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() } }
