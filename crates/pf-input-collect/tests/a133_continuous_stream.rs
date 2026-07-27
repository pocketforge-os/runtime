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

fn meta() -> DeviceMeta { DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() } }
