//! The wizard driver: it owns the render loop + the event pump and drives the headless
//! `pf_input_collect` engine's [`Collector`] state machine (record → commit → advance/back → emit),
//! rendering a frame — the REAL DEVICE with the prompted control highlighted (from the gated
//! `.scad`->skin assets, via [`SkinSet`]) — for each step. Two entry points share the same frame
//! presentation:
//!  - [`drive_live`] — engine-parity pump against a real [`EventSource`] (a live evdev node). The
//!    real-collection path (full live-pad exercise is tsp-bwrg.6).
//!  - [`drive_demo`] — an auto-advancing pass that synthesizes a press per control against a
//!    scripted source, so the FULL render + prompt sequence is demonstrable on the panel WITHOUT
//!    the live pad decoder. This proves this bead's on-panel acceptance.

use std::time::Duration;

use pf_input_collect::codes::{EV_ABS, EV_KEY};
use pf_input_collect::collect::DeviceMeta;
use pf_input_collect::emit::Capabilities;
use pf_input_collect::source::{AbsInfo, EventSource, Identity, RawEvent, ScriptedSource};
use pf_input_collect::{collect::CollectError, plan, Collector};

use crate::canvas::Canvas;
use crate::render::{render_frame, FrameState, CANVAS_H, CANVAS_W};
use crate::skin::SkinSet;

pub const TITLE: &str = "POCKETFORGE INPUT COLLECTION";

/// A frame sink — where a rendered canvas goes (the panel, or nowhere in a test).
pub trait Sink {
    fn present(&mut self, canvas: &Canvas);
}

/// Discards frames — for host-side tests of the drive logic.
pub struct NullSink;
impl Sink for NullSink {
    fn present(&mut self, _c: &Canvas) {}
}

/// Counts frames presented — lets a test assert the wizard actually rendered each step.
#[derive(Default)]
pub struct CountingSink {
    pub frames: usize,
}
impl Sink for CountingSink {
    fn present(&mut self, _c: &Canvas) {
        self.frames += 1;
    }
}

/// Timing/heuristic knobs. `pre_dwell`/`post_dwell` make each step visible on-panel (the demo sets
/// them; live leaves them zero and lets the real press provide the timing). The pump knobs mirror
/// `pf_input_collect::collect::RunConfig`.
#[derive(Clone, Copy)]
pub struct Timing {
    pub poll_step: Duration,
    pub quiet_polls: usize,
    pub idle_skip_polls: usize,
    pub max_polls: usize,
    pub pre_dwell: Duration,
    pub post_dwell: Duration,
}

impl Timing {
    /// Live: engine-parity, no artificial dwell.
    pub fn live() -> Timing {
        Timing {
            poll_step: Duration::from_millis(50),
            quiet_polls: 3,
            idle_skip_polls: 40,
            max_polls: 600,
            pre_dwell: Duration::ZERO,
            post_dwell: Duration::ZERO,
        }
    }

    /// Demo: hold each control long enough for a person / the webcam to see it.
    pub fn demo() -> Timing {
        Timing {
            pre_dwell: Duration::from_millis(950),
            post_dwell: Duration::from_millis(650),
            ..Timing::live()
        }
    }
}

/// The per-frame knobs that change step to step (index/total come from the collector).
struct StepView<'a> {
    active_id: Option<&'a str>,
    prompt: &'a str,
    status: &'a str,
    done: bool,
}

/// Render + present one frame from the collector's current state.
fn present<K: Sink>(sink: &mut K, canvas: &mut Canvas, skin: &SkinSet, collector: &Collector, v: StepView) {
    let (i, n) = collector.position();
    let st = FrameState {
        title: TITLE,
        active_id: v.active_id,
        prompt: v.prompt,
        index: i + 1,
        total: n,
        status: v.status,
        done: v.done,
    };
    render_frame(canvas, skin, &st);
    sink.present(canvas);
}

fn relevant(kind: plan::Kind, e: &RawEvent) -> bool {
    match kind {
        plan::Kind::Button | plan::Kind::StickClick => e.ev_type == EV_KEY,
        plan::Kind::Hat | plan::Kind::Stick => e.ev_type == EV_ABS,
        // A trigger may be analog (ABS_Z/RZ) OR a binary button (the a133 L2/R2 → BTN_TL2/BTN_TR2),
        // so either an axis or a key press counts as activity — mirrors pf_input_collect::is_relevant.
        plan::Kind::Trigger => e.ev_type == EV_ABS || e.ev_type == EV_KEY,
    }
}

/// Drive the full guided sequence against a live source, rendering each step. Engine-parity pump.
pub fn drive_live<S: EventSource, K: Sink>(
    src: &mut S,
    sink: &mut K,
    skin: &SkinSet,
    meta: &DeviceMeta,
    timing: &Timing,
) -> Result<Capabilities, CollectError> {
    let mut collector = Collector::new(plan::default_gamepad_plan());
    let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);

    while let Some(spec) = collector.current().cloned() {
        present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &spec.prompt, status: "PRESS THE HIGHLIGHTED CONTROL", done: false });

        let mut buf: Vec<RawEvent> = Vec::new();
        let mut saw = false;
        let mut empties = 0usize;
        let mut showed_capturing = false;
        for _ in 0..timing.max_polls {
            let evs = src.poll(timing.poll_step).map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
            if evs.is_empty() {
                empties += 1;
                if !saw {
                    if spec.optional && empties >= timing.idle_skip_polls {
                        break;
                    }
                } else if empties >= timing.quiet_polls {
                    break;
                }
                continue;
            }
            empties = 0;
            for e in evs {
                if relevant(spec.kind, &e) {
                    saw = true;
                }
                buf.push(e);
            }
            if saw && !showed_capturing {
                present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &spec.prompt, status: "CAPTURING...", done: false });
                showed_capturing = true;
            }
            // A button/click — and a trigger realized as a binary button (a133 L2/R2) — completes
            // the instant a key-down is seen. A genuinely analog trigger emits no key, so its sweep
            // is never short-circuited.
            if matches!(spec.kind, plan::Kind::Button | plan::Kind::StickClick | plan::Kind::Trigger)
                && buf.iter().any(|e| e.ev_type == EV_KEY && e.value == 1)
            {
                break;
            }
        }

        collector.record(&buf);
        let _ = collector.commit_current(src)?;
        collector.advance();
        present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: &spec.prompt, status: "PRESS THE HIGHLIGHTED CONTROL", done: collector.is_done() });
        if !timing.post_dwell.is_zero() {
            std::thread::sleep(timing.post_dwell);
        }
    }

    present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: "COLLECTION COMPLETE", status: "EMITTING CANDIDATE CAPABILITIES.TOML", done: true });
    collector.emit(src, meta)
}

/// Drive the sequence with a SYNTHETIC press per control — the on-panel render/advance proof that
/// needs no live pad. `src` provides identity + absinfo for commit/emit; events are fed directly.
pub fn drive_demo<K: Sink>(
    src: &mut ScriptedSource,
    sink: &mut K,
    skin: &SkinSet,
    meta: &DeviceMeta,
    timing: &Timing,
) -> Result<Capabilities, CollectError> {
    let mut collector = Collector::new(plan::default_gamepad_plan());
    let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);

    while let Some(spec) = collector.current().cloned() {
        present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &spec.prompt, status: "GUIDED COLLECTION (DEMO) - AUTO-ADVANCING", done: false });
        if !timing.pre_dwell.is_zero() {
            std::thread::sleep(timing.pre_dwell);
        }

        let evs = synth_events_for(&spec.id);
        if !evs.is_empty() {
            present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &spec.prompt, status: "CAPTURING...", done: false });
            collector.record(&evs);
        }
        let _ = collector.commit_current(src)?;
        collector.advance();
        present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: &spec.prompt, status: "GUIDED COLLECTION (DEMO) - AUTO-ADVANCING", done: collector.is_done() });
        if !timing.post_dwell.is_zero() {
            std::thread::sleep(timing.post_dwell);
        }
    }

    present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: "COLLECTION COMPLETE", status: "EMITTING CANDIDATE CAPABILITIES.TOML", done: true });
    collector.emit(src, meta)
}

/// A scripted source shaped like a canonical gamepad — identity + the declared axis ranges the
/// demo's stick/trigger captures need for `EVIOCGABS`. Used only by the demo/dump paths.
pub fn demo_source() -> ScriptedSource {
    let ident = Identity { name: "PocketForge Demo Pad".into(), bus: 0x03, vid: 0x1209, pid: 0xb163, version: 0x0100 };
    let stick = AbsInfo { min: 0, max: 4095, fuzz: 16, flat: 128, resolution: 0 };
    let trig = AbsInfo { min: 0, max: 255, fuzz: 0, flat: 0, resolution: 0 };
    let hat = AbsInfo { min: -1, max: 1, fuzz: 0, flat: 0, resolution: 0 };
    ScriptedSource::new(ident)
        .with_abs(0x0, stick) // ABS_X
        .with_abs(0x1, stick) // ABS_Y
        .with_abs(0x3, stick) // ABS_RX
        .with_abs(0x4, stick) // ABS_RY
        .with_abs(0x2, trig) // ABS_Z  (left trigger)
        .with_abs(0x5, trig) // ABS_RZ (right trigger)
        .with_abs(0x10, hat) // ABS_HAT0X
        .with_abs(0x11, hat) // ABS_HAT0Y
}

/// The synthetic events a demo press produces for a plan id. Optional controls left with no
/// hardware (`guide`, `l3`, `r3`) return empty → the engine records them as Skipped (row omitted).
fn synth_events_for(id: &str) -> Vec<RawEvent> {
    let btn = |code: u16| vec![RawEvent::new(EV_KEY, code, 1), RawEvent::new(EV_KEY, code, 0)];
    let stick = |xc: u16, yc: u16| {
        let mut v = Vec::new();
        for &val in &[2048, 4095, 2048, 0, 2048] {
            v.push(RawEvent::new(EV_ABS, xc, val));
        }
        for &val in &[2048, 4095, 2048, 0, 2048] {
            v.push(RawEvent::new(EV_ABS, yc, val));
        }
        v
    };
    match id {
        "south" => btn(0x130),
        "east" => btn(0x131),
        "west" => btn(0x133),
        "north" => btn(0x134),
        "select" => btn(0x13a),
        "start" => btn(0x13b),
        "l1" => btn(0x136),
        "r1" => btn(0x137),
        "dpad" => vec![
            RawEvent::new(EV_ABS, 0x10, -1),
            RawEvent::new(EV_ABS, 0x10, 1),
            RawEvent::new(EV_ABS, 0x11, -1),
            RawEvent::new(EV_ABS, 0x11, 1),
        ],
        "lstick" => stick(0x0, 0x1),
        "rstick" => stick(0x3, 0x4),
        // Left trigger realized as a binary BUTTON — the real a133 L2/R2 shape (the MCU reports it
        // as a bit; the decoder emits BTN_TL2). Exercises the button-trigger path on the panel.
        "ltrig" => btn(0x138), // BTN_TL2
        // Right trigger ANALOG (intermediate travel) — shows the other classification.
        "rtrig" => vec![
            RawEvent::new(EV_ABS, 0x5, 0),
            RawEvent::new(EV_ABS, 0x5, 90),
            RawEvent::new(EV_ABS, 0x5, 180),
            RawEvent::new(EV_ABS, 0x5, 255),
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Rgb;
    use crate::skin::{Rect, View};
    use std::collections::HashMap;

    // A tiny synthetic skin covering every a133 engine id so compose() never no-ops in the drive.
    fn demo_skin() -> SkinSet {
        let ids = [
            ("south", "btn_south"), ("east", "btn_east"), ("west", "btn_west"), ("north", "btn_north"),
            ("select", "btn_select"), ("start", "btn_start"), ("guide", "btn_guide"), ("l1", "btn_l1"),
            ("r1", "btn_r1"), ("dpad", "dpad"), ("lstick", "stick_l"), ("rstick", "stick_r"),
            ("ltrig", "trig_l"), ("rtrig", "trig_r"),
        ];
        let body = Rgb::new(60, 30, crate::canvas::rgb(248, 248, 248));
        let lit = Rgb::new(60, 30, crate::canvas::rgb(210, 0, 0));
        let mut parts = HashMap::new();
        let mut map = HashMap::new();
        for (i, (id, part)) in ids.iter().enumerate() {
            parts.insert(part.to_string(), Rect { x: (i as i64) * 2, y: 0, w: 2, h: 2 });
            map.insert(id.to_string(), part.to_string());
        }
        SkinSet::from_parts(map, View { body, lit, parts }, None, crate::canvas::rgb(248, 248, 248))
    }

    #[test]
    fn demo_runs_the_full_sequence_and_emits_a_valid_candidate() {
        let mut src = demo_source();
        let mut sink = CountingSink::default();
        let skin = demo_skin();
        let meta = DeviceMeta { id: "demopad".into(), manufacturer: "PocketForge".into(), model: "Demo Pad".into() };
        let timing = Timing { pre_dwell: Duration::ZERO, post_dwell: Duration::ZERO, ..Timing::demo() };
        let caps = drive_demo(&mut src, &mut sink, &skin, &meta, &timing).expect("demo should emit");

        assert!(sink.frames >= 14, "expected a frame per control, got {}", sink.frames);
        let toml = caps.to_toml();
        assert!(toml.contains("id = \"south\""), "candidate missing south:\n{toml}");
        assert!(toml.contains("id = \"ltrig\""));
        assert!(toml.contains("semantics = \"binary\""), "left trigger should classify binary:\n{toml}");
        assert!(toml.contains("semantics = \"analog\""), "right trigger should classify analog:\n{toml}");
        assert!(!toml.contains("id = \"guide\""), "skipped guide must be omitted:\n{toml}");
    }

    #[test]
    fn drive_live_captures_a_button_from_a_scripted_stream() {
        let ident = Identity { name: "x".into(), bus: 3, vid: 1, pid: 1, version: 1 };
        let mut src = ScriptedSource::new(ident);
        src.push_batch(vec![RawEvent::new(EV_KEY, 0x130, 1), RawEvent::new(EV_KEY, 0x130, 0)]);
        let mut sink = CountingSink::default();
        let skin = demo_skin();
        let meta = DeviceMeta { id: "x".into(), manufacturer: "x".into(), model: "x".into() };
        let timing = Timing { max_polls: 2, idle_skip_polls: 1, quiet_polls: 1, ..Timing::live() };
        let err = drive_live(&mut src, &mut sink, &skin, &meta, &timing).unwrap_err();
        match err {
            CollectError::NoActivity { id } => assert_ne!(id, "south", "south should have captured"),
            other => panic!("unexpected: {other}"),
        }
        assert!(sink.frames >= 2, "should have rendered at least the south prompt + result");
    }
}
