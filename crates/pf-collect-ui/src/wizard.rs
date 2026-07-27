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

use std::time::{Duration, Instant};

use pf_input_collect::codes::{EV_ABS, EV_KEY};
use pf_input_collect::collect::{self, CommitOutcome, DeviceMeta};
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

    /// Observe the SEMANTIC content of the frame being presented (the status line, prompt, and
    /// highlighted control) — a default no-op so the on-panel/dump sinks are untouched. The
    /// status line is the operator's ONLY feedback channel, so a test sink overrides this to pin
    /// the wizard's state PROGRESSION (idle → CAPTURING → RECOGNIZED / re-prompt), which the
    /// rendered pixels cannot be asserted on cheaply. Called for every frame `present` emits.
    fn observe_frame(&mut self, _status: &str, _prompt: &str, _active_id: Option<&str>) {}
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
    /// Runaway guard (NOT the timing bound — that is `control_timeout`).
    pub max_polls: usize,
    /// Wall-clock per-control window — the real bound. A poll count is unreliable on a device that
    /// STREAMS continuously (the a133 pad ~48fps): `poll()` never blocks so a fixed count burns
    /// through in seconds, far too short for a person to react (tsp-bwrg.6). Generous = err long.
    pub control_timeout: Duration,
    /// Polls discarded in the gap between two controls — a FIXED dead-time window
    /// (`pf_input_collect::collect::drain_between_controls`).
    pub drain_polls: usize,
    pub pre_dwell: Duration,
    pub post_dwell: Duration,
}

/// The per-control wall-clock window, defaulting to `default_secs` but overridable at runtime with
/// `PF_COLLECT_UI_CONTROL_TIMEOUT_S`. The default (45s) is the generous window a person needs during
/// a real collection; the override widens it for an owner-parked wizard that waits far longer per
/// control (the owner walks up and works through the controls on their own schedule) and for a
/// stable on-panel hold during verification. A non-positive / unparseable value falls back to the default.
fn control_timeout_from_env(default_secs: u64) -> Duration {
    let secs = std::env::var("PF_COLLECT_UI_CONTROL_TIMEOUT_S")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

/// A demo dwell (milliseconds) read from `var`, defaulting to `default_ms`. The demo's per-step
/// pre/post dwell is overridable (used by demorun.sh) so an operator can widen it — e.g. hold one
/// control frame long enough for a stable on-panel display check. An unparseable value falls back
/// to the default; `0` is honored (an explicit no-dwell).
fn demo_dwell_from_env(var: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

impl Timing {
    /// Live: engine-parity, no artificial dwell.
    pub fn live() -> Timing {
        let engine = collect::RunConfig::default();
        Timing {
            poll_step: engine.poll_step,
            quiet_polls: engine.quiet_polls,
            idle_skip_polls: engine.idle_skip_polls,
            max_polls: engine.max_polls,                    // runaway guard only
            control_timeout: control_timeout_from_env(45),  // 45s default; err long
            drain_polls: engine.drain_polls,
            pre_dwell: Duration::ZERO,
            post_dwell: Duration::ZERO,
        }
    }

    /// The engine [`collect::RunConfig`] these knobs express. The wizard drives the SAME completion
    /// policy the headless engine does (`collect::Window`), so its knobs must reach that policy
    /// rather than be re-interpreted by a second loop here — see `drive_live` (tsp-bwrg.12).
    pub fn run_config(&self) -> collect::RunConfig {
        collect::RunConfig {
            poll_step: self.poll_step,
            quiet_polls: self.quiet_polls,
            idle_skip_polls: self.idle_skip_polls,
            control_timeout: self.control_timeout,
            drain_polls: self.drain_polls,
            max_polls: self.max_polls,
        }
    }

    /// Demo: hold each control long enough for a person / the webcam to see it. The dwells are
    /// overridable via PF_COLLECT_UI_PRE_DWELL_MS / PF_COLLECT_UI_POST_DWELL_MS (used by demorun.sh
    /// and to widen a single-frame hold past the webcam capture lag for display verification).
    pub fn demo() -> Timing {
        Timing {
            pre_dwell: demo_dwell_from_env("PF_COLLECT_UI_PRE_DWELL_MS", 950),
            post_dwell: demo_dwell_from_env("PF_COLLECT_UI_POST_DWELL_MS", 650),
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
    sink.observe_frame(v.status, v.prompt, v.active_id);
    sink.present(canvas);
}

/// The clarifying re-prompt shown when a control did NOT capture on the first try — a partial
/// stick sweep (only one axis), a hat with only one axis, or a missed press. It names exactly what
/// to add so the owner isn't left guessing (tsp-bwrg.6: the parked wizard must recover a fumble in
/// place, never abort). Sticks and hats both need BOTH axes actuated.
fn reprompt_hint(spec: &plan::ControlSpec) -> String {
    // ASCII only — the 5x7 bitmap font has no em-dash (it renders the fallback hollow box, which the
    // owner saw on pass #5). Use a plain hyphen.
    match spec.kind {
        plan::Kind::Stick => "Almost - roll the stick ONE full circle, touching every edge".to_string(),
        plan::Kind::HatDir => "Press that D-PAD direction firmly".to_string(),
        plan::Kind::Hat => "Press the OTHER directions too - I need UP/DOWN and LEFT/RIGHT".to_string(),
        _ => format!("Didn't catch that - {}", spec.prompt),
    }
}

/// Append the device's PRINTED faceplate glyph (descriptor `label`) to a control's prompt — e.g.
/// "Press the BOTTOM face button (B)" on a Nintendo-arranged chassis. The glyph comes SOLELY from
/// the descriptor, never an SDL letter derived from the internal position id: rendering an internal
/// convention instead of the device's own printed name is the tsp-bwrg.6 pass-#5 A/B defect (and the
/// phantom-HOME defect before it). Controls the descriptor gives no `label` (sticks, triggers,
/// select/start) keep their positional prompt unchanged.
fn labeled_prompt(base: String, skin: &SkinSet, id: &str) -> String {
    match skin.label_for(id) {
        Some(lbl) => format!("{base} ({lbl})"),
        None => base,
    }
}

/// The positive acknowledgment shown AFTER a control is captured and BEFORE the next prompt — the
/// success half of the wizard's feedback vocabulary. Until tsp-bwrg.15 the wizard's only state
/// feedback was the FAILURE re-prompt (`reprompt_hint` / "LET'S TRY THAT ONE AGAIN"); a successful
/// press was SILENT, so a captured press and a not-yet-registered one looked identical until the
/// screen changed (owner-observed on the live 17-prompt run). This NAMES the control just
/// recognized — the operator's real question is "did it get the RIGHT one?", acute given the
/// face-button frame history (tsp-ozbp.14) — using the device's PRINTED faceplate glyph (descriptor
/// `label`, e.g. "B" on a Nintendo-arranged chassis) when it has one, else the positional id
/// (SELECT, LSTICK, DPAD UP). ASCII only — the 5x7 bitmap font renders a hollow box for non-ASCII
/// (the em-dash the owner saw on pass #5), so no dash/arrow glyphs.
///
/// `pub(crate)` so the headless `--dump-dir` render ([`crate::dump`]) shows the identical ack state
/// the panel does — one source of the wording, not two.
pub(crate) fn ack_status(skin: &SkinSet, id: &str) -> String {
    let name = skin
        .label_for(id)
        .map(|l| l.to_string())
        .unwrap_or_else(|| id.to_uppercase().replace('_', " "));
    format!("RECOGNIZED: {name}")
}

/// Drive the full guided sequence against a live source, rendering each step. Engine-parity pump.
pub fn drive_live<S: EventSource, K: Sink>(
    src: &mut S,
    sink: &mut K,
    skin: &SkinSet,
    meta: &DeviceMeta,
    timing: &Timing,
) -> Result<Capabilities, CollectError> {
    let mut collector = Collector::new(plan::a133_gamepad_plan());
    let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);

    while let Some(spec) = collector.current().cloned() {
        // Owner-paced RETRY: re-prompt the SAME control until it actually captures. A fumble — a
        // partial stick sweep (one axis), a hat with only one axis, a mis-press — must NEVER abort
        // the run and discard the controls already collected (tsp-bwrg.6: an lstick partial sweep
        // discarded ten good controls). It just re-prompts, with a clarifying hint. Only a HARD
        // engine error (an unknown code, an EVIOCGABS failure) aborts.
        let mut attempt = 0usize;
        // Set exactly once, when a control commits (the `break` arm below); the loop only exits via
        // that break or an early `return` on a hard error, so it is definitely-assigned after.
        let outcome: CommitOutcome;
        loop {
            attempt += 1;
            let base = if attempt == 1 { spec.prompt.clone() } else { reprompt_hint(&spec) };
            let prompt_text = labeled_prompt(base, skin, &spec.id);
            let status = if attempt == 1 { "PRESS THE HIGHLIGHTED CONTROL" } else { "LET'S TRY THAT ONE AGAIN" };
            present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &prompt_text, status, done: false });

            // The ONE completion policy, CONSUMED from the engine — never a second copy here
            // (tsp-bwrg.12). This loop owns rendering and nothing else about when a control is done:
            // duplicated-truth-with-no-drift-check is how the quiet-based bug survived in the UI
            // after the engine had moved on.
            let cfg = timing.run_config();
            let mut window = collect::Window::new(&spec);
            let mut showed_capturing = false;
            // WALL-CLOCK per-control window — the a133 streams continuously so a poll count would burn
            // through in seconds (tsp-bwrg.6). `max_polls` is only a runaway guard now.
            let deadline = Instant::now() + timing.control_timeout;
            let mut iters = 0usize;
            // Keep-alive: re-present the idle prompt periodically. On a long parked timeout a control
            // is otherwise drawn only ONCE, and that single present can lose the panel to a stale
            // frame left by the outgoing menu (tsp-bwrg.6: the parked wizard was alive but the panel
            // kept showing the menu). Periodic re-presents make the panel converge to the current
            // control within ~1.5s regardless, and keep a parked panel live.
            let mut last_present = Instant::now();
            let keepalive = std::time::Duration::from_millis(1500);
            while Instant::now() < deadline && iters < timing.max_polls {
                iters += 1;
                let evs = src.poll(timing.poll_step).map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
                let actuated_now = window.observe(&evs, src);
                if actuated_now && !showed_capturing {
                    present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &prompt_text, status: "CAPTURING...", done: false });
                    showed_capturing = true;
                    last_present = Instant::now();
                }
                // Keep-alive re-present of the idle prompt (see `last_present`/`keepalive` above).
                if !showed_capturing && last_present.elapsed() >= keepalive {
                    present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &prompt_text, status, done: false });
                    last_present = Instant::now();
                }
                match window.verdict(&cfg) {
                    collect::Verdict::Open => {}
                    collect::Verdict::Complete | collect::Verdict::Skip => break,
                }
            }

            collector.record(window.events());
            match collector.commit_current(src) {
                // Captured — or an OPTIONAL control that saw nothing was cleanly skipped. Advance.
                Ok(o) => {
                    outcome = o;
                    break;
                }
                // Not enough of this control was actuated — RE-PROMPT it (never abort+discard). The
                // next loop iteration re-presents with `reprompt_hint`. commit_current already took
                // the working buffer on error; clear_working is belt-and-suspenders.
                Err(CollectError::NoActivity { .. }) | Err(CollectError::Incomplete { .. }) => {
                    collector.clear_working();
                    continue;
                }
                // A hard engine error (unknown code / absinfo failure) is not owner-recoverable.
                Err(e) => return Err(e),
            }
        }
        collector.advance();
        // POSITIVE ACK (tsp-bwrg.15). A successful capture was previously silent — the only state
        // feedback the wizard had was the FAILURE re-prompt, so "advanced" was the only sign a press
        // registered. Show an explicit "RECOGNIZED: <control>" naming what was captured, with the
        // control still highlighted, BEFORE the next prompt.
        //
        // ⚠ Rendered in the tsp-bwrg.12 inter-control seam — the double-tap / stale-queue drain
        // class lives here. The ack is a PURE RENDER: it adds NO sleep and consumes NO poll. Its
        // on-screen dwell is BORROWED from the `drain_between_controls` window that runs right after
        // (a FIXED dead-time window that already DISCARDS input in this gap, safe by construction),
        // so there is deliberately no independent ack-dwell constant to swallow or defer the next
        // control's first input. A blocking sleep or a poll-consuming dwell HERE would reintroduce
        // exactly the class this seam guards — see the no-event-lost test in this module.
        match outcome {
            CommitOutcome::Captured(_) => {
                let status = ack_status(skin, &spec.id);
                let ack_prompt = labeled_prompt(spec.prompt.clone(), skin, &spec.id);
                present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &ack_prompt, status: &status, done: collector.is_done() });
            }
            // An optional control that produced nothing is an OMISSION, not a capture — no positive
            // ack (there is nothing "recognized"). Keep the prior neutral advance frame.
            CommitOutcome::Skipped => {
                present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: &spec.prompt, status: "PRESS THE HIGHLIGHTED CONTROL", done: collector.is_done() });
            }
        }
        if !timing.post_dwell.is_zero() {
            std::thread::sleep(timing.post_dwell);
        }
        // Discard this control's tail before the NEXT control is prompted, so an overshoot or a
        // "did that take?" re-press cannot satisfy the next prompt (tsp-bwrg.12). Deliberately
        // AFTER the post-dwell + ack: the backlog the device buffered while that frame was held is
        // exactly what would otherwise be the next control's first poll. The owner has not been
        // asked the next question yet, so nothing arriving here can be an answer to it. The ack
        // frame above stays on-panel for the whole of this window (its dwell).
        if !collector.is_done() {
            collect::drain_between_controls(src, &timing.run_config())
                .map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
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
    let mut collector = Collector::new(plan::a133_gamepad_plan());
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
        let outcome = collector.commit_current(src)?;
        collector.advance();
        // Mirror drive_live's positive ack (tsp-bwrg.15) so the demo/on-panel review shows the new
        // state too. The demo drives a scripted source with no inter-control drain, so this is a
        // plain render frame held for `post_dwell`.
        match outcome {
            CommitOutcome::Captured(_) => {
                let status = ack_status(skin, &spec.id);
                present(sink, &mut canvas, skin, &collector, StepView { active_id: Some(&spec.id), prompt: &spec.prompt, status: &status, done: collector.is_done() });
            }
            CommitOutcome::Skipped => {
                present(sink, &mut canvas, skin, &collector, StepView { active_id: None, prompt: &spec.prompt, status: "GUIDED COLLECTION (DEMO) - AUTO-ADVANCING", done: collector.is_done() });
            }
        }
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
        // Four atomic dpad directions (each a single hat-axis actuation): up/down -> HAT0Y (0x11),
        // left/right -> HAT0X (0x10). They merge to one hat row at emit.
        "dpad_up" => vec![RawEvent::new(EV_ABS, 0x11, -1), RawEvent::new(EV_ABS, 0x11, 0)],
        "dpad_down" => vec![RawEvent::new(EV_ABS, 0x11, 1), RawEvent::new(EV_ABS, 0x11, 0)],
        "dpad_left" => vec![RawEvent::new(EV_ABS, 0x10, -1), RawEvent::new(EV_ABS, 0x10, 0)],
        "dpad_right" => vec![RawEvent::new(EV_ABS, 0x10, 1), RawEvent::new(EV_ABS, 0x10, 0)],
        "lstick" => stick(0x0, 0x1),
        "rstick" => stick(0x3, 0x4),
        // Left trigger realized as a binary BUTTON — the real a133 L2/R2 shape (the MCU reports it
        // as a bit; the decoder emits BTN_TL2). Exercises the button-trigger path on the panel.
        "guide" => btn(0x13c), // BTN_MODE (the MENU button; SDL `guide`)
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
    fn prompt_renders_the_descriptor_label_never_a_position_letter() {
        // The on-panel prompt for a face button carries the device's PRINTED glyph from the
        // descriptor `label` (south="B" on this Nintendo chassis), NOT an SDL letter derived from the
        // internal "south" id. A regression to an SDL letter (or dropping the label) FAILS here
        // instead of pointing the owner at the wrong button (tsp-bwrg.6 pass-#5 A/B fix).
        let mut labels = HashMap::new();
        labels.insert("south".to_string(), "B".to_string());
        labels.insert("east".to_string(), "A".to_string());
        let skin = demo_skin().with_labels(labels);
        assert_eq!(labeled_prompt("Press the BOTTOM face button".into(), &skin, "south"), "Press the BOTTOM face button (B)");
        assert_eq!(labeled_prompt("Press the RIGHT face button".into(), &skin, "east"), "Press the RIGHT face button (A)");
        // A control the descriptor gives no label keeps its positional prompt unchanged.
        assert_eq!(labeled_prompt("Press SELECT".into(), &skin, "select"), "Press SELECT");
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
        assert!(toml.contains("id = \"guide\""), "the MENU button (guide) must be captured, not skipped:\n{toml}");
    }

    /// The re-prompt fix (tsp-bwrg.6): a control that does not capture on the first try (here SOUTH,
    /// whose window elapses before the press) must be RE-PROMPTED — never abort the run and discard
    /// the controls already collected. Drives the full a133 plan to completion with south fumbled
    /// once, and asserts the run returns Ok with EVERY control (including the fumbled south and the
    /// MENU/guide button) captured.
    ///
    /// HUMAN-TIMED FIXTURE (re-timed in tsp-bwrg.12). This script previously packed one batch per
    /// control with NO separator between most of them — machine-timed input, the fixture profile
    /// that lets a completion bug hide (tsp-bwrg.6 named finding). It also could not survive the
    /// inter-control drain, which by design consumes the gap between two controls: with no gap to
    /// consume, the drain would eat the NEXT control's only batch. Every control now gets its own
    /// actuation followed by real quiet, exactly as a person produces.
    ///
    /// Face-button codes come from `pf_input_decode::codes` — POSITIONAL (Frame C), never a letter
    /// (tsp-ozbp.14 / runtime#33): on this Nintendo-arranged chassis a glyph-keyed constant is right
    /// exactly half the time.
    #[test]
    fn drive_live_reprompts_a_fumbled_control_and_keeps_prior_captures() {
        use pf_input_decode::codes::{
            BTN_EAST, BTN_MODE, BTN_NORTH, BTN_SELECT, BTN_SOUTH, BTN_START, BTN_TL, BTN_TL2,
            BTN_TR, BTN_TR2, BTN_WEST,
        };
        const EV_ABS: u16 = 0x03;
        const QUIET: usize = 12; // per-control trailing quiet: the settle, THEN the fixed drain
        let stick = AbsInfo { min: 0, max: 4095, fuzz: 0, flat: 0, resolution: 0 };
        let hat = AbsInfo { min: -1, max: 1, fuzz: 0, flat: 0, resolution: 0 };
        let ident = Identity { name: "a133".into(), bus: 3, vid: 0x045e, pid: 0x028e, version: 0x0110 };
        let mut src = ScriptedSource::new(ident)
            .with_abs(0x0, stick).with_abs(0x1, stick).with_abs(0x3, stick).with_abs(0x4, stick)
            .with_abs(0x10, hat).with_abs(0x11, hat);
        let press = |code: u16| vec![RawEvent::new(EV_KEY, code, 1), RawEvent::new(EV_KEY, code, 0)];
        let hatdir = |code: u16, v: i32| vec![RawEvent::new(EV_ABS, code, v), RawEvent::new(EV_ABS, code, 0)];
        let abs2 = |ca: u16, cb: u16, v: i32| vec![RawEvent::new(EV_ABS, ca, v), RawEvent::new(EV_ABS, cb, v)];
        // max_polls bounds each attempt's window; the FUMBLE is that many quiet polls with no press.
        const MAX_POLLS: usize = 8;
        let quiet = |s: &mut ScriptedSource, n: usize| { for _ in 0..n { s.push_batch(vec![]); } };

        // south — FUMBLE: the window elapses with no activity, forcing NoActivity -> re-prompt ...
        quiet(&mut src, MAX_POLLS);
        // ... then the press lands on the retry.
        src.push_batch(press(BTN_SOUTH));
        quiet(&mut src, QUIET);
        for code in [BTN_EAST, BTN_WEST, BTN_NORTH, BTN_SELECT, BTN_START, BTN_MODE, BTN_TL, BTN_TR] {
            src.push_batch(press(code));
            quiet(&mut src, QUIET);
        }
        // dpad — FOUR atomic direction steps (HatDir): up/down actuate HAT0Y, left/right HAT0X.
        // They MERGE at emit into the single hat row.
        for (code, v) in [(0x11u16, -1), (0x11, 1), (0x10, -1), (0x10, 1)] {
            src.push_batch(hatdir(code, v));
            quiet(&mut src, QUIET);
        }
        // lstick + rstick — a real full sweep: BOTH axes to BOTH extremes (the coverage gate), which
        // a single-corner deflection does not satisfy.
        for (cx, cy) in [(0x0u16, 0x1u16), (0x3, 0x4)] {
            src.push_batch(abs2(cx, cy, 4095));
            src.push_batch(abs2(cx, cy, 0));
            quiet(&mut src, QUIET);
        }
        for code in [BTN_TL2, BTN_TR2] {
            src.push_batch(press(code)); // binary triggers, realized as buttons
            quiet(&mut src, QUIET);
        }

        let mut sink = CountingSink::default();
        let skin = demo_skin();
        let meta = DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() };
        // control_timeout kept short so a mis-fed control fails fast in-test rather than idling 45s.
        let timing = Timing { max_polls: MAX_POLLS, idle_skip_polls: 1, quiet_polls: 1, post_dwell: Duration::ZERO, control_timeout: Duration::from_secs(2), ..Timing::live() };

        let caps = drive_live(&mut src, &mut sink, &skin, &meta, &timing)
            .expect("a fumbled control must re-prompt and the run must complete, not abort");
        let ids: Vec<&str> = caps.inputs.iter().map(|i| i.id.as_str()).collect();
        // The fumbled south survived the re-prompt, and the MENU/guide button was captured.
        for want in ["south", "east", "west", "north", "select", "start", "guide", "l1", "r1",
            "dpad", "lstick", "rstick", "ltrig", "rtrig"]
        {
            assert!(ids.contains(&want), "control {want} missing from the completed run: {ids:?}");
        }
        // The dpad merged to the two HAT axes — not a stick axis picked up as a direction.
        let dpad = caps.inputs.iter().find(|i| i.id == "dpad").expect("dpad row");
        assert_eq!(dpad.code, "ABS_HAT0X,ABS_HAT0Y");
    }

    /// A sink that records the STATUS line of every frame (the operator's only feedback channel),
    /// so a test can assert the wizard's state PROGRESSION — not just the emitted candidate.
    #[derive(Default)]
    struct RecordingSink {
        statuses: Vec<String>,
    }
    impl Sink for RecordingSink {
        fn present(&mut self, _c: &Canvas) {}
        fn observe_frame(&mut self, status: &str, _prompt: &str, _active: Option<&str>) {
            self.statuses.push(status.to_string());
        }
    }

    /// Build a full-a133-plan scripted source in ONE pass (no fragile re-polling). Control order is
    /// the plan's: south, east, west, north, select, start, guide, l1, r1, four dpad dirs, l/r stick,
    /// l/r trigger — every control fed its OWN positional code so the emitted map is per-control
    /// checkable. Knobs:
    ///  - `fumble_south`: prepend `fumble_polls` dead polls so south's FIRST window elapses
    ///    (NoActivity -> re-prompt) before its real press lands — exercises the re-prompt path.
    ///  - `quiet_gap`: empty polls after each control (>= drain_polls, so every boundary keeps
    ///    enough slack that the drain never eats the NEXT control's press).
    ///  - `ltrig_gap`: empty polls after ltrig (the SECOND-TO-LAST control), i.e. the gap BEFORE
    ///    rtrig. Set to the engine `drain_polls` for a NO-CUSHION boundary — the drain consumes
    ///    exactly these, so rtrig's window opens directly on rtrig's press and a poll-consuming ack
    ///    would swallow it. Deliberately the LAST boundary: a swallowed rtrig press shifts only
    ///    rtrig (nothing follows it to cascade into), so the regression fails on a clean wrong-code
    ///    assertion rather than a type-mismatch re-prompt hang (a Button window fed a hat event).
    ///  - `pad`: trailing (press, quiet) pairs so a shifted rtrig still finds a press (the run
    ///    COMPLETES and fails on a wrong code, instead of starving the last control into a hang).
    fn scripted_a133(fumble_south: bool, quiet_gap: usize, ltrig_gap: usize, fumble_polls: usize, pad: usize) -> ScriptedSource {
        use pf_input_decode::codes::{
            BTN_EAST, BTN_MODE, BTN_NORTH, BTN_SELECT, BTN_SOUTH, BTN_START, BTN_TL, BTN_TL2,
            BTN_TR, BTN_TR2, BTN_WEST,
        };
        const EV_ABS: u16 = 0x03;
        let stick = AbsInfo { min: 0, max: 4095, fuzz: 0, flat: 0, resolution: 0 };
        let hat = AbsInfo { min: -1, max: 1, fuzz: 0, flat: 0, resolution: 0 };
        let ident = Identity { name: "a133".into(), bus: 3, vid: 0x045e, pid: 0x028e, version: 0x0110 };
        let mut src = ScriptedSource::new(ident)
            .with_abs(0x0, stick).with_abs(0x1, stick).with_abs(0x3, stick).with_abs(0x4, stick)
            .with_abs(0x10, hat).with_abs(0x11, hat);
        let press = |code: u16| vec![RawEvent::new(EV_KEY, code, 1), RawEvent::new(EV_KEY, code, 0)];
        let hatdir = |code: u16, v: i32| vec![RawEvent::new(EV_ABS, code, v), RawEvent::new(EV_ABS, code, 0)];
        let abs2 = |ca: u16, cb: u16, v: i32| vec![RawEvent::new(EV_ABS, ca, v), RawEvent::new(EV_ABS, cb, v)];
        let quiet = |s: &mut ScriptedSource, n: usize| { for _ in 0..n { s.push_batch(vec![]); } };

        if fumble_south {
            quiet(&mut src, fumble_polls); // south's first window elapses -> re-prompt
        }
        for code in [BTN_SOUTH, BTN_EAST, BTN_WEST, BTN_NORTH, BTN_SELECT, BTN_START, BTN_MODE, BTN_TL, BTN_TR] {
            src.push_batch(press(code));
            quiet(&mut src, quiet_gap);
        }
        for (code, v) in [(0x11u16, -1), (0x11, 1), (0x10, -1), (0x10, 1)] {
            src.push_batch(hatdir(code, v));
            quiet(&mut src, quiet_gap);
        }
        for (cx, cy) in [(0x0u16, 0x1u16), (0x3, 0x4)] {
            src.push_batch(abs2(cx, cy, 4095));
            src.push_batch(abs2(cx, cy, 0));
            quiet(&mut src, quiet_gap);
        }
        // ltrig -> rtrig is the tested boundary: ltrig's cushion is `ltrig_gap` (tight = drain_polls).
        src.push_batch(press(BTN_TL2));
        quiet(&mut src, ltrig_gap);
        src.push_batch(press(BTN_TR2));
        quiet(&mut src, quiet_gap);
        for _ in 0..pad {
            src.push_batch(press(BTN_SOUTH));
            quiet(&mut src, quiet_gap);
        }
        src
    }

    /// The timing tsp-bwrg.12's drive_live test uses: a short wall-clock window + small poll counts
    /// so a mis-fed control fails fast in-test rather than idling 45s. `drain_polls` comes from
    /// `Timing::live()` (the engine default, 8) — the value the fixtures are gapped against.
    fn fast_test_timing() -> Timing {
        Timing { max_polls: 8, idle_skip_polls: 1, quiet_polls: 1, post_dwell: Duration::ZERO, control_timeout: Duration::from_secs(2), ..Timing::live() }
    }

    /// tsp-bwrg.15 acceptance #3 — the positive ack is shown ON a successful capture and NOT on the
    /// re-prompt (failure) path. south is FUMBLED once (its first window elapses with no input,
    /// forcing a NoActivity re-prompt), then captured on the retry. We assert on the recorded
    /// status sequence that:
    ///   - "RECOGNIZED: ..." appears EXACTLY ONCE PER CONTROL (17), never an 18th from the fumble —
    ///     an ack leaking onto the re-prompt path would push this to 18.
    ///   - the re-prompt path WAS exercised ("LET'S TRY THAT ONE AGAIN" present), so that count is
    ///     meaningful, and the first ack comes strictly AFTER that re-prompt (a capture, not a
    ///     re-prompt, is what produces it).
    ///   - the ack NAMES the control (south, and a stick) — acceptance #1.
    ///
    /// This FAILS against the pre-change build, which emitted no "RECOGNIZED" status at all (success
    /// was silent) — so the very first assertion goes red for the right reason.
    #[test]
    fn positive_ack_is_shown_on_capture_and_not_on_the_reprompt_path() {
        const MAX_POLLS: usize = 8;
        const QUIET: usize = 12;

        // south fumbled once (its first window elapses), captured on the retry; all others clean.
        let mut src = scripted_a133(true, QUIET, QUIET, MAX_POLLS, 0);
        let mut sink = RecordingSink::default();
        let skin = demo_skin(); // no faceplate labels -> the ack falls back to the positional id
        let meta = DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() };
        let timing = Timing { max_polls: MAX_POLLS, ..fast_test_timing() };

        let caps = drive_live(&mut src, &mut sink, &skin, &meta, &timing)
            .expect("the fumbled south must re-prompt and the run must complete");
        assert_eq!(caps.inputs.len(), 14, "all 17 prompts collapse to 14 rows (4 dpad dirs merge)"); // sanity

        let recognized: Vec<&String> = sink.statuses.iter().filter(|s| s.starts_with("RECOGNIZED")).collect();
        assert!(
            !recognized.is_empty(),
            "the wizard emitted NO 'RECOGNIZED' status — success is still silent (pre-change behaviour); \
             statuses seen: {:?}",
            sink.statuses
        );
        assert_eq!(
            recognized.len(),
            17,
            "expected exactly one positive ack per CAPTURED control (17) — an 18th means the ack \
             leaked onto the fumbled south's re-prompt path. acks: {recognized:?}"
        );
        // The re-prompt path was actually exercised (so 'not 18' is a real signal, not luck).
        let reprompt_at = sink.statuses.iter().position(|s| s == "LET'S TRY THAT ONE AGAIN")
            .expect("south should have fumbled and shown the re-prompt");
        let first_ack_at = sink.statuses.iter().position(|s| s.starts_with("RECOGNIZED")).unwrap();
        assert!(
            first_ack_at > reprompt_at,
            "the first ack (idx {first_ack_at}) must come AFTER south's re-prompt (idx {reprompt_at}) — \
             it is produced by the CAPTURE, never by the re-prompt"
        );
        // The ack NAMES the control it recognized (acceptance #1).
        assert!(sink.statuses.iter().any(|s| s == "RECOGNIZED: SOUTH"), "ack should name south: {:?}", recognized);
        assert!(sink.statuses.iter().any(|s| s == "RECOGNIZED: LSTICK"), "ack should name the left stick: {:?}", recognized);
    }

    /// tsp-bwrg.15 acceptance #4 — the REGRESSION THAT MATTERS: no event is lost across the ack.
    /// ltrig's press is followed by EXACTLY `drain_polls` empty polls (which the inter-control drain
    /// consumes) and then rtrig's press with NO further cushion — so rtrig's window opens directly on
    /// rtrig's press. The ack is a pure render that consumes no poll, so BOTH are captured with their
    /// own codes. A blocking / poll-consuming ack (a dwell that polls the source in this seam) would
    /// swallow rtrig's press; rtrig would then capture the PAD press instead and record the wrong
    /// code. Proven live against the regression: injecting one `src.poll()` into the ack seam makes
    /// this test fail `rtrig got BTN_A, want BTN_TR2` (recorded in the bead), while the real
    /// pure-render ack passes. The tested boundary is the LAST one on purpose — a swallowed rtrig
    /// press shifts only rtrig (nothing follows to cascade into), so the failure is a clean wrong-code
    /// assertion, not a type-mismatch re-prompt hang. The pad guarantees the shifted rtrig still
    /// finds a press so the run COMPLETES and fails on the assertion.
    ///
    /// Anchored on POSITION (`pf_input_decode::codes`, Frame C) fed in, translated to the emitted
    /// Frame-D name via `pf_input_collect::codes::key_name` — never a bare glyph letter (tsp-ozbp.14).
    #[test]
    fn no_event_is_lost_across_the_positive_ack() {
        use pf_input_collect::codes::key_name;
        use pf_input_decode::codes::{BTN_TL2, BTN_TR2};

        let timing = fast_test_timing();
        let drain = timing.drain_polls; // the gap the inter-control drain consumes (engine default 8)
        const QUIET: usize = 12;

        // Tight (no-cushion) ltrig -> rtrig boundary: after ltrig's press, EXACTLY `drain` empties,
        // then rtrig's press. A pad guards against a hang if a regressed ack shifts rtrig's capture.
        let mut src = scripted_a133(false, QUIET, drain, 0, 4);
        let mut sink = CountingSink::default();
        let skin = demo_skin();
        let meta = DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() };

        let caps = drive_live(&mut src, &mut sink, &skin, &meta, &timing)
            .expect("the run must complete");

        let code_of = |id: &str| caps.inputs.iter().find(|i| i.id == id).map(|i| i.code.clone());
        // Every control landed (the ack lost nobody's event) and both trigger rows are present.
        assert_eq!(caps.inputs.len(), 14, "all 17 prompts collapse to 14 rows (4 dpad dirs merge)");
        // ltrig recorded its own press...
        assert_eq!(
            code_of("ltrig").as_deref(),
            key_name(BTN_TL2),
            "ltrig lost or shifted its capture; got {:?}",
            code_of("ltrig")
        );
        // ...and rtrig recorded ITS OWN press across the no-cushion ack boundary — the crux. A
        // poll-consuming ack would have swallowed rtrig's press and rtrig would carry the pad's code.
        assert_eq!(
            code_of("rtrig").as_deref(),
            key_name(BTN_TR2),
            "rtrig did NOT record its own press across the no-cushion ack boundary — the ack swallowed \
             rtrig's event (a blocking/poll-consuming ack). rtrig got {:?}",
            code_of("rtrig")
        );
    }
}
