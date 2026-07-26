//! A backend-agnostic in-memory canvas: an RGB pixel buffer plus the 2D primitives the collection
//! face needs (rects, circles, the 5x7 font). The SAME draw code fills this buffer whether the
//! sink is `/dev/fb0` on-device or a PPM file off-device — the render is proven headless before
//! the device is ever touched, and the on-panel frame is pixel-identical to the validated one.

use crate::font::{self, GLYPH_H, GLYPH_W};

/// A packed 0x00RRGGBB color.
pub type Color = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

#[inline]
pub fn channels(c: Color) -> (u8, u8, u8) {
    (((c >> 16) & 0xff) as u8, ((c >> 8) & 0xff) as u8, (c & 0xff) as u8)
}

/// An RGB software canvas.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    px: Vec<Color>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h] }
    }

    /// Raw pixel access (row-major, `y * w + x`), for a framebuffer blit.
    pub fn pixels(&self) -> &[Color] {
        &self.px
    }

    pub fn clear(&mut self, c: Color) {
        for p in self.px.iter_mut() {
            *p = c;
        }
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, c: Color) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = c;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, c);
            }
        }
    }

    /// A rectangle outline `t` pixels thick, drawn inward from the given bounds.
    pub fn rect_outline(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, c: Color) {
        self.fill_rect(x, y, w, t, c); // top
        self.fill_rect(x, y + h - t, w, t, c); // bottom
        self.fill_rect(x, y, t, h, c); // left
        self.fill_rect(x + w - t, y, t, h, c); // right
    }

    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, c: Color) {
        let r2 = r * r;
        for yy in -r..=r {
            for xx in -r..=r {
                if xx * xx + yy * yy <= r2 {
                    self.put(cx + xx, cy + yy, c);
                }
            }
        }
    }

    /// A ring: an annulus between radius `r - t` and `r`.
    pub fn ring(&mut self, cx: i32, cy: i32, r: i32, t: i32, c: Color) {
        let ro2 = r * r;
        let ri = (r - t).max(0);
        let ri2 = ri * ri;
        for yy in -r..=r {
            for xx in -r..=r {
                let d = xx * xx + yy * yy;
                if d <= ro2 && d >= ri2 {
                    self.put(cx + xx, cy + yy, c);
                }
            }
        }
    }

    /// Draw one 5x7 glyph with its top-left at (x, y), each source pixel a `scale`x`scale` block.
    pub fn glyph(&mut self, x: i32, y: i32, ch: char, scale: usize, c: Color) {
        let rows = font::glyph(ch);
        let s = scale as i32;
        for (ry, row) in rows.iter().enumerate() {
            for cx in 0..GLYPH_W {
                // bit (GLYPH_W-1 - cx) is the pixel for column cx (bit 4 = leftmost).
                if row & (1 << (GLYPH_W - 1 - cx)) != 0 {
                    self.fill_rect(x + cx as i32 * s, y + ry as i32 * s, s, s, c);
                }
            }
        }
    }

    /// Draw a string left-aligned at (x, y). Returns the x just past the last glyph.
    pub fn text(&mut self, x: i32, y: i32, s: &str, scale: usize, c: Color) -> i32 {
        let step = (GLYPH_W + 1) as i32 * scale as i32; // one column gap between glyphs
        let mut cx = x;
        for ch in s.chars() {
            self.glyph(cx, y, ch, scale, c);
            cx += step;
        }
        cx
    }

    /// Draw a string horizontally centered on `center_x`.
    pub fn text_centered(&mut self, center_x: i32, y: i32, s: &str, scale: usize, c: Color) {
        let w = font::text_width(s, scale) as i32;
        self.text(center_x - w / 2, y, s, scale, c);
    }

    pub const GLYPH_H: usize = GLYPH_H;

    /// Serialize as a binary PPM (P6) — the off-device validation sink (`--dump`). Also the
    /// exact bytes a screenshot reviewer sees, so what CI validates IS what the panel will show.
    pub fn to_ppm(&self) -> Vec<u8> {
        let header = format!("P6\n{} {}\n255\n", self.w, self.h);
        let mut out = Vec::with_capacity(header.len() + self.w * self.h * 3);
        out.extend_from_slice(header.as_bytes());
        for &p in &self.px {
            let (r, g, b) = channels(p);
            out.push(r);
            out.push(g);
            out.push(b);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_read_back() {
        let mut c = Canvas::new(4, 3);
        c.clear(rgb(0, 0, 0));
        c.put(1, 1, rgb(255, 128, 64));
        assert_eq!(c.pixels()[4 + 1], rgb(255, 128, 64)); // (x=1,y=1) in a width-4 canvas
        // out-of-bounds put is a no-op, not a panic
        c.put(-1, 0, rgb(1, 1, 1));
        c.put(4, 0, rgb(1, 1, 1));
    }

    #[test]
    fn ppm_header_and_size() {
        let c = Canvas::new(2, 2);
        let ppm = c.to_ppm();
        assert!(ppm.starts_with(b"P6\n2 2\n255\n"));
        // header + 2*2 pixels * 3 bytes
        assert_eq!(ppm.len(), b"P6\n2 2\n255\n".len() + 12);
    }

    #[test]
    fn fill_rect_paints_the_right_pixels() {
        let mut c = Canvas::new(5, 5);
        c.clear(rgb(0, 0, 0));
        c.fill_rect(1, 1, 2, 2, rgb(255, 255, 255));
        assert_eq!(c.pixels()[5 + 1], rgb(255, 255, 255)); // (1,1)
        assert_eq!(c.pixels()[2 * 5 + 2], rgb(255, 255, 255)); // (2,2)
        assert_eq!(c.pixels()[0], rgb(0, 0, 0));
        assert_eq!(c.pixels()[3 * 5 + 3], rgb(0, 0, 0)); // (3,3), outside the rect
    }
}
