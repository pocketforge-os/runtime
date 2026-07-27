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

/// Render one highlighted frame per plan control (plus a completion frame) to `dir` as PPMs, using
/// the consumed device skin. Returns the written paths in order.
pub fn dump_frames(dir: &Path, skin: &SkinSet) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let controls = plan::a133_gamepad_plan();
    let total = controls.len();
    let mut written = Vec::new();

    for (i, spec) in controls.iter().enumerate() {
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
        let p = dir.join(format!("{:02}-{}.ppm", i + 1, spec.id));
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
    let p = dir.join(format!("{:02}-done.ppm", total + 1));
    std::fs::write(&p, canvas.to_ppm())?;
    written.push(p);

    Ok(written)
}
