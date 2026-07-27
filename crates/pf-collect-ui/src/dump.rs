//! Headless frame dump: render the full prompt sequence to PPM files WITHOUT a device, so the
//! render (the real device with the correct control highlighted per prompt) is validated by the
//! screenshot reviewer before the panel is ever touched. The pixels dumped here are byte-identical
//! to what the fbdev sink blits on-panel — validate here, trust there.

use std::io;
use std::path::{Path, PathBuf};

use pf_input_collect::plan;

use crate::canvas::Canvas;
use crate::render::{render_frame, FrameState, CANVAS_H, CANVAS_W};
use crate::skin::SkinSet;
use crate::wizard::TITLE;

/// Render the prompt AND the positive-ack frame for each plan control (plus a completion frame) to
/// `dir` as PPMs, using the consumed device skin. Returns the written paths in order.
///
/// TWO frames per control since tsp-bwrg.15: the `-prompt` frame (idle, "press this") and the
/// `-recognized` frame (the positive ack the wizard now shows on a successful capture, before it
/// advances — [`crate::wizard::ack_status`]). Rendering the ack here is what makes the new state
/// reviewable HEADLESSLY by the screenshot reviewer before the panel is touched. Frame count is
/// therefore `2 * controls + 1` — pinned by `dump_frames_renders_prompt_and_ack_per_control`, so a
/// future change to the per-control frame set trips that test rather than drifting silently.
pub fn dump_frames(dir: &Path, skin: &SkinSet) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let controls = plan::a133_gamepad_plan();
    let total = controls.len();
    let mut written = Vec::new();

    for (i, spec) in controls.iter().enumerate() {
        // The prompt (idle "press the highlighted control") frame.
        let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
        let st = FrameState {
            title: TITLE,
            active_id: Some(&spec.id),
            prompt: &spec.prompt,
            index: i + 1,
            total,
            status: "RENDER VALIDATION - HEADLESS (NO DEVICE)",
            done: false,
        };
        render_frame(&mut canvas, skin, &st);
        let p = dir.join(format!("{:02}-{}-prompt.ppm", i + 1, spec.id));
        std::fs::write(&p, canvas.to_ppm())?;
        written.push(p);

        // The positive-ack ("RECOGNIZED: <control>") frame — the tsp-bwrg.15 new state, with the
        // just-captured control still highlighted, exactly as the on-panel wizard renders it.
        let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
        let ack = crate::wizard::ack_status(skin, &spec.id);
        let st = FrameState {
            title: TITLE,
            active_id: Some(&spec.id),
            prompt: &spec.prompt,
            index: i + 1,
            total,
            status: &ack,
            done: false,
        };
        render_frame(&mut canvas, skin, &st);
        let p = dir.join(format!("{:02}-{}-recognized.ppm", i + 1, spec.id));
        std::fs::write(&p, canvas.to_ppm())?;
        written.push(p);
    }

    // Completion frame (neutral device).
    let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
    let st = FrameState {
        title: TITLE,
        active_id: None,
        prompt: "COLLECTION COMPLETE",
        index: total,
        total,
        status: "EMITTING CANDIDATE CAPABILITIES.TOML",
        done: true,
    };
    render_frame(&mut canvas, skin, &st);
    // Numbered after the last per-control ack frame (2*total) so the ordinals stay contiguous.
    let p = dir.join(format!("{:02}-done.ppm", 2 * total + 1));
    std::fs::write(&p, canvas.to_ppm())?;
    written.push(p);

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Rgb;
    use crate::skin::View;
    use std::collections::HashMap;

    /// A minimal SkinSet — a solid body/lit atlas, no part rects. `dump_frames` only needs a skin
    /// that renders; the per-control highlight no-ops on an unmapped id, which is fine for counting
    /// and shape assertions here (the real-asset render path is covered by `a133_real_skin.rs`).
    fn tiny_skin() -> SkinSet {
        let body = Rgb::new(60, 30, crate::canvas::rgb(248, 248, 248));
        let lit = Rgb::new(60, 30, crate::canvas::rgb(210, 0, 0));
        let view = View { body, lit, parts: HashMap::new() };
        SkinSet::from_parts(HashMap::new(), view, None, crate::canvas::rgb(248, 248, 248))
    }

    /// The dump renders a PROMPT frame AND a RECOGNIZED (positive-ack) frame per control, plus one
    /// completion frame: `2 * controls + 1` (tsp-bwrg.15). Before this bead the dump was one frame
    /// per control + done (`controls + 1`) and success had no rendered state at all. Pinning the
    /// exact count + the `-recognized.ppm` filenames makes acceptance #5 explicit: a future change
    /// to the per-control frame set must update THIS assertion deliberately, not drift silently
    /// (the "check-shaped absence keeps reading as coverage" trap — there was no count fixture
    /// before, so the ack could have been added to the panel while the headless review artifact
    /// showed nothing).
    #[test]
    fn dump_frames_renders_prompt_and_ack_per_control() {
        let n = plan::a133_gamepad_plan().len();
        let dir = std::env::temp_dir().join(format!("pf-collect-ui-dump-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = dump_frames(&dir, &tiny_skin()).expect("dump_frames should write the PPMs");

        assert_eq!(
            written.len(),
            2 * n + 1,
            "expected a prompt + a recognized frame per control ({n}) plus one done frame = {}, got {}",
            2 * n + 1,
            written.len(),
        );
        // Every control contributes a positive-ack frame — the new state is actually in the dump.
        let recognized = written.iter().filter(|p| p.to_string_lossy().ends_with("-recognized.ppm")).count();
        assert_eq!(recognized, n, "expected one -recognized.ppm per control, got {recognized}");
        let prompts = written.iter().filter(|p| p.to_string_lossy().ends_with("-prompt.ppm")).count();
        assert_eq!(prompts, n, "expected one -prompt.ppm per control, got {prompts}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
