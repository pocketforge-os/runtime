//! Frame composition: draw the REAL DEVICE (composited from the gated `.scad`->skin assets with the
//! prompted control highlighted) with the title, progress readout, prompt, and status overlaid. The
//! device image comes from [`crate::skin::SkinSet::compose`]; this module owns only the on-canvas
//! layout + the text overlay. The fbdev/dump layer just gets these pixels onto the panel or a PPM.

use crate::canvas::{rgb, Canvas, Color};
use crate::font;
use crate::skin::SkinSet;

/// The logical canvas the layout is authored against (letterboxed to the real fb resolution).
pub const CANVAS_W: i32 = 1280;
pub const CANVAS_H: i32 = 720;

// Layout: the device fills the middle band; title above, prompt/status below.
const DEVICE_Y: i32 = 78;
const DEVICE_H: i32 = 468;
const DEVICE_MARGIN_X: i32 = 24;

// Palette.
const BG: Color = rgb(18, 20, 28);
const TEXT: Color = rgb(234, 238, 245);
const ACTIVE: Color = rgb(255, 196, 64);
const DONE_C: Color = rgb(58, 196, 120);
const TEXT_DIM: Color = rgb(150, 158, 172);
const TITLE_C: Color = rgb(120, 200, 255);

/// Everything the renderer needs for one frame.
pub struct FrameState<'a> {
    pub title: &'a str,
    /// The engine control id currently prompted (highlighted on the device), or `None`.
    pub active_id: Option<&'a str>,
    pub prompt: &'a str,
    /// 1-based current control number.
    pub index: usize,
    pub total: usize,
    pub status: &'a str,
    pub done: bool,
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

/// Fit `(iw,ih)` into `(area_w,area_h)` preserving aspect. Returns (draw_w, draw_h).
fn fit(iw: i32, ih: i32, area_w: i32, area_h: i32) -> (i32, i32) {
    if iw <= 0 || ih <= 0 {
        return (0, 0);
    }
    let by_h_w = (area_h as i64 * iw as i64 / ih as i64) as i32;
    if by_h_w <= area_w {
        (by_h_w, area_h)
    } else {
        (area_w, (area_w as i64 * ih as i64 / iw as i64) as i32)
    }
}

/// Draw one full frame. `c` MUST be `CANVAS_W`x`CANVAS_H`.
pub fn render_frame(c: &mut Canvas, skin: &SkinSet, st: &FrameState) {
    debug_assert_eq!((c.w as i32, c.h as i32), (CANVAS_W, CANVAS_H));
    c.clear(BG);

    // Title + progress.
    c.text_centered(CANVAS_W / 2, 22, st.title, 3, TITLE_C);
    let progress = if st.done { "DONE".to_string() } else { format!("{} / {}", st.index, st.total) };
    c.text(CANVAS_W - font::text_width(&progress, 3) as i32 - 22, 22, &progress, 3, TEXT);

    // The real device, with the prompted control highlighted (neutral when done).
    let img = skin.compose(if st.done { None } else { st.active_id });
    let (dw, dh) = fit(img.w as i32, img.h as i32, CANVAS_W - 2 * DEVICE_MARGIN_X, DEVICE_H);
    let dx = (CANVAS_W - dw) / 2;
    let dy = DEVICE_Y + (DEVICE_H - dh) / 2;
    c.blit_scaled_keyed(&img, dx, dy, dw, dh, Some((skin.bg, 12)));

    // Prompt (wrapped) + status.
    let prompt_scale = 4usize;
    let prompt_c = if st.done { DONE_C } else { ACTIVE };
    let lines = wrap(st.prompt, CANVAS_W - 120, prompt_scale);
    let line_h = (Canvas::GLYPH_H + 3) as i32 * prompt_scale as i32;
    let mut y = 560;
    for line in &lines {
        c.text_centered(CANVAS_W / 2, y, line, prompt_scale, prompt_c);
        y += line_h;
    }
    c.text_centered(CANVAS_W / 2, CANVAS_H - 28, st.status, 2, TEXT_DIM);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Rgb;
    use crate::skin::{Rect, SkinSet, View};
    use std::collections::HashMap;

    fn tiny_skin() -> SkinSet {
        let body = Rgb::new(40, 20, rgb(248, 248, 248)); // light bg (color-keyed out)
        let mut lit = Rgb::new(40, 20, rgb(248, 248, 248));
        for y in 8..12 {
            for x in 18..24 {
                lit.set(x, y, rgb(210, 0, 0));
            }
        }
        let mut parts = HashMap::new();
        parts.insert("btn_south".to_string(), Rect { x: 18, y: 8, w: 6, h: 4 });
        let mut map = HashMap::new();
        map.insert("south".to_string(), "btn_south".to_string());
        SkinSet::from_parts(map, View { body, lit, parts }, None, rgb(248, 248, 248))
    }

    #[test]
    fn renders_device_and_overlay_without_panicking() {
        let mut c = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
        let skin = tiny_skin();
        let st = FrameState {
            title: "POCKETFORGE INPUT COLLECTION",
            active_id: Some("south"),
            prompt: "Press the BOTTOM face button (south)",
            index: 1,
            total: 14,
            status: "demo",
            done: false,
        };
        render_frame(&mut c, &skin, &st);
        // Not all background — something was drawn.
        assert!(c.pixels().iter().any(|&p| p != BG));
        // The red highlight (color-keyed device) should appear somewhere in the device band.
        let mut saw_red = false;
        for y in DEVICE_Y..DEVICE_Y + DEVICE_H {
            for x in 0..CANVAS_W {
                let (r, g, b) = crate::canvas::channels(c.pixels()[y as usize * c.w + x as usize]);
                if r > 150 && g < 80 && b < 80 {
                    saw_red = true;
                }
            }
        }
        assert!(saw_red, "the highlighted control's red should be visible on the device");
    }

    #[test]
    fn fit_preserves_aspect() {
        // 1480x640 into 1232x468 -> fit by height: 468*1480/640 = 1082 <= 1232
        assert_eq!(fit(1480, 640, 1232, 468), (1082, 468));
        // a very wide image fits by width instead
        let (w, h) = fit(4000, 400, 1232, 468);
        assert_eq!(w, 1232);
        assert!(h < 468);
    }

    #[test]
    fn wrap_breaks_long_prompts() {
        let long = "Sweep the LEFT STICK fully in a circle all the way in every direction";
        let lines = wrap(long, CANVAS_W - 120, 4);
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(font::text_width(l, 4) as i32 <= CANVAS_W - 120);
        }
    }
}
