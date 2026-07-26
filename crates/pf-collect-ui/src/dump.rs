//! Headless frame dump: render the full prompt sequence to PPM files WITHOUT a device, so the
//! render is validated (correct face, correct control highlighted per prompt) by the screenshot
//! reviewer before the panel is ever touched. The pixels dumped here are byte-identical to what
//! the fbdev sink blits on-panel — validate here, trust there.

use std::io;
use std::path::{Path, PathBuf};

use pf_input_collect::plan;

use crate::canvas::Canvas;
use crate::face::{CANVAS_H, CANVAS_W};
use crate::render::{render_frame, FrameState};
use crate::wizard::TITLE;

/// The optional controls the demo/dump treats as absent hardware (skipped, greyed, row omitted).
fn is_demo_skipped(id: &str) -> bool {
    matches!(id, "guide" | "l3" | "r3")
}

/// Render one highlighted frame per plan control (plus a completion frame) to `dir` as PPMs,
/// showing captured progress accumulating. Returns the written paths in order.
pub fn dump_frames(dir: &Path) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let controls = plan::default_gamepad_plan();
    let total = controls.len();
    let mut recorded: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut written = Vec::new();

    for (i, spec) in controls.iter().enumerate() {
        let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
        let st = FrameState {
            title: TITLE,
            active_id: Some(&spec.id),
            recorded_ids: &recorded,
            skipped_ids: &skipped,
            prompt: &spec.prompt,
            index: i + 1,
            total,
            status: "RENDER VALIDATION - HEADLESS (NO DEVICE)",
            done: false,
        };
        render_frame(&mut canvas, &st);
        let p = dir.join(format!("{:02}-{}.ppm", i + 1, spec.id));
        std::fs::write(&p, canvas.to_ppm())?;
        written.push(p);

        if is_demo_skipped(&spec.id) {
            skipped.push(spec.id.clone());
        } else {
            recorded.push(spec.id.clone());
        }
    }

    // Completion frame.
    let mut canvas = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
    let st = FrameState {
        title: TITLE,
        active_id: None,
        recorded_ids: &recorded,
        skipped_ids: &skipped,
        prompt: "COLLECTION COMPLETE",
        index: total,
        total,
        status: "EMITTING CANDIDATE CAPABILITIES.TOML",
        done: true,
    };
    render_frame(&mut canvas, &st);
    let p = dir.join(format!("{:02}-done.ppm", total + 1));
    std::fs::write(&p, canvas.to_ppm())?;
    written.push(p);

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_one_frame_per_control_plus_done() {
        let dir = std::env::temp_dir().join(format!("pf-collect-ui-dump-test-{}", std::process::id()));
        let paths = dump_frames(&dir).expect("dump should succeed");
        // 16 controls + 1 done frame.
        assert_eq!(paths.len(), plan::default_gamepad_plan().len() + 1);
        // Each file is a non-trivial PPM.
        for p in &paths {
            let bytes = std::fs::read(p).unwrap();
            assert!(bytes.starts_with(b"P6\n1280 720\n255\n"), "{p:?} not a 1280x720 PPM");
            assert!(bytes.len() > 1280 * 720, "{p:?} suspiciously small");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
