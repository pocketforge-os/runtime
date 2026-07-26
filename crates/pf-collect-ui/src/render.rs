//! Frame composition: given the current wizard state, draw one 1280x720 frame — the title, the
//! progress readout, the generic gamepad face (with the currently-prompted control HIGHLIGHTED and
//! already-captured controls marked), the prompt text, and a status line. The fbdev/dump layer is
//! responsible only for getting these pixels onto the panel or into a PPM; all layout lives here.

use crate::canvas::{rgb, Canvas, Color};
use crate::face::{self, Control, Shape, CANVAS_H, CANVAS_W};
use crate::font;

// Palette.
const BG: Color = rgb(18, 20, 28);
const BODY: Color = rgb(40, 44, 56);
const IDLE_FILL: Color = rgb(58, 63, 76);
const IDLE_LINE: Color = rgb(120, 128, 142);
const RECORDED: Color = rgb(58, 196, 120);
const SKIPPED: Color = rgb(78, 82, 96);
const ACTIVE: Color = rgb(255, 196, 64);
const GLOW: Color = rgb(255, 224, 130);
const TEXT: Color = rgb(234, 238, 245);
const TEXT_DIM: Color = rgb(150, 158, 172);
const TITLE_C: Color = rgb(120, 200, 255);

/// Everything the renderer needs for one frame. Ids are borrowed slices so no allocation per frame.
pub struct FrameState<'a> {
    pub title: &'a str,
    pub active_id: Option<&'a str>,
    pub recorded_ids: &'a [String],
    pub skipped_ids: &'a [String],
    pub prompt: &'a str,
    /// 1-based current control number (0 when done).
    pub index: usize,
    pub total: usize,
    pub status: &'a str,
    pub done: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Idle,
    Recorded,
    Skipped,
    Active,
}

fn style_for(ctrl: &Control, st: &FrameState) -> Style {
    if st.active_id == Some(ctrl.id) {
        Style::Active
    } else if st.recorded_ids.iter().any(|r| r == ctrl.id) {
        Style::Recorded
    } else if st.skipped_ids.iter().any(|s| s == ctrl.id) {
        Style::Skipped
    } else {
        Style::Idle
    }
}

fn colors(style: Style) -> (Color, Color) {
    // (fill, outline)
    match style {
        Style::Idle => (IDLE_FILL, IDLE_LINE),
        Style::Recorded => (RECORDED, rgb(220, 255, 232)),
        Style::Skipped => (SKIPPED, rgb(110, 116, 130)),
        Style::Active => (ACTIVE, GLOW),
    }
}

fn draw_control(c: &mut Canvas, ctrl: &Control, style: Style) {
    let (fill, line) = colors(style);
    match ctrl.shape {
        Shape::Circle { r } => {
            if style == Style::Active {
                c.ring(ctrl.cx, ctrl.cy, r + 12, 5, GLOW);
            }
            c.fill_circle(ctrl.cx, ctrl.cy, r, fill);
            c.ring(ctrl.cx, ctrl.cy, r, 3, line);
        }
        Shape::Rect { w, h } => {
            let (x, y) = (ctrl.cx - w / 2, ctrl.cy - h / 2);
            if style == Style::Active {
                c.rect_outline(x - 8, y - 8, w + 16, h + 16, 4, GLOW);
            }
            c.fill_rect(x, y, w, h, fill);
            c.rect_outline(x, y, w, h, 3, line);
        }
        Shape::Cross { arm, thick } => {
            if style == Style::Active {
                c.rect_outline(ctrl.cx - arm - 8, ctrl.cy - thick / 2 - 8, 2 * arm + 16, thick + 16, 4, GLOW);
                c.rect_outline(ctrl.cx - thick / 2 - 8, ctrl.cy - arm - 8, thick + 16, 2 * arm + 16, 4, GLOW);
            }
            // horizontal arm
            c.fill_rect(ctrl.cx - arm, ctrl.cy - thick / 2, 2 * arm, thick, fill);
            // vertical arm
            c.fill_rect(ctrl.cx - thick / 2, ctrl.cy - arm, thick, 2 * arm, fill);
            c.rect_outline(ctrl.cx - arm, ctrl.cy - thick / 2, 2 * arm, thick, 2, line);
            c.rect_outline(ctrl.cx - thick / 2, ctrl.cy - arm, thick, 2 * arm, 2, line);
        }
    }
    // Label: inside the control (wide bars / face buttons) or below it. Pick a label color that
    // contrasts with the fill it sits on — dark on the bright active/recorded fills, light on the
    // dark idle fill.
    let half_glyph = (Canvas::GLYPH_H as i32 * 2) / 2; // scale-2 glyph half-height
    if ctrl.label_inside {
        let inside_c = if matches!(style, Style::Active | Style::Recorded) {
            rgb(20, 22, 28)
        } else {
            TEXT
        };
        c.text_centered(ctrl.cx, ctrl.cy - half_glyph, ctrl.label, 2, inside_c);
    } else {
        let label_c = if style == Style::Active { TEXT } else { TEXT_DIM };
        let below = match ctrl.shape {
            Shape::Circle { r } => r + 6,
            Shape::Rect { h, .. } => h / 2 + 6,
            Shape::Cross { arm, .. } => arm + 6,
        };
        c.text_centered(ctrl.cx, ctrl.cy + below, ctrl.label, 2, label_c);
    }
}

/// Break `s` into lines that each fit within `max_px` at `scale`.
fn wrap(s: &str, max_px: i32, scale: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let trial = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if font::text_width(&trial, scale) as i32 <= max_px {
            cur = trial;
        } else {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Draw one full frame. `c` MUST be `CANVAS_W`x`CANVAS_H` (the face coordinates are absolute).
pub fn render_frame(c: &mut Canvas, st: &FrameState) {
    debug_assert_eq!((c.w as i32, c.h as i32), (CANVAS_W, CANVAS_H));
    c.clear(BG);

    // Title + progress bar.
    c.text_centered(CANVAS_W / 2, 24, st.title, 3, TITLE_C);
    let progress = if st.done {
        "DONE".to_string()
    } else {
        format!("{} / {}", st.index, st.total)
    };
    c.text(CANVAS_W - font::text_width(&progress, 3) as i32 - 24, 24, &progress, 3, TEXT);

    // Controller body.
    c.fill_rect(330, 150, 620, 340, BODY);
    c.rect_outline(330, 150, 620, 340, 3, rgb(70, 76, 92));

    // Controls: draw non-active first so the active glow lands on top; l3/r3 after their stick.
    let face = face::generic_face();
    let mut active: Option<&Control> = None;
    for ctrl in &face {
        let style = style_for(ctrl, st);
        if style == Style::Active {
            active = Some(ctrl);
        } else {
            draw_control(c, ctrl, style);
        }
    }
    if let Some(ctrl) = active {
        draw_control(c, ctrl, Style::Active);
    }

    // Prompt block (wrapped), amber when a control is active, dim when done.
    let prompt_scale = 4usize;
    let prompt_c = if st.done { RECORDED } else { ACTIVE };
    let lines = wrap(st.prompt, CANVAS_W - 120, prompt_scale);
    let line_h = (Canvas::GLYPH_H + 3) as i32 * prompt_scale as i32;
    let mut y = 545;
    for line in &lines {
        c.text_centered(CANVAS_W / 2, y, line, prompt_scale, prompt_c);
        y += line_h;
    }

    // Status hint line at the very bottom.
    c.text_centered(CANVAS_W / 2, CANVAS_H - 30, st.status, 2, TEXT_DIM);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_bg(c: &Canvas) -> bool {
        c.pixels().iter().all(|&p| p == BG)
    }

    #[test]
    fn renders_something_and_active_highlight_changes_pixels() {
        let mut c = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
        let recorded: Vec<String> = vec!["south".into()];
        let st = FrameState {
            title: "POCKETFORGE INPUT COLLECTION",
            active_id: Some("east"),
            recorded_ids: &recorded,
            skipped_ids: &[],
            prompt: "Press the RIGHT face button (east)",
            index: 2,
            total: 16,
            status: "auto-advancing demo",
            done: false,
        };
        render_frame(&mut c, &st);
        assert!(!is_bg(&c), "frame should not be all background");

        // The amber glow should appear somewhere near the 'east' hotspot (895, 265).
        let near = |cx: usize, cy: usize| -> bool {
            let mut found = false;
            for dy in 0..40usize {
                for dx in 0..40usize {
                    let x = cx + dx - 20;
                    let y = cy + dy - 20;
                    if x < c.w && y < c.h && c.pixels()[y * c.w + x] == ACTIVE {
                        found = true;
                    }
                }
            }
            found
        };
        assert!(near(895, 265), "active control 'east' should paint ACTIVE pixels near its hotspot");
    }

    #[test]
    fn wrap_breaks_long_prompts() {
        let long = "Sweep the LEFT STICK fully in a circle all the way in every direction";
        let lines = wrap(long, CANVAS_W - 120, 4);
        assert!(lines.len() >= 2, "a long prompt should wrap to multiple lines");
        for l in &lines {
            assert!(font::text_width(l, 4) as i32 <= CANVAS_W - 120);
        }
    }
}
