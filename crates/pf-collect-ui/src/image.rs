//! A tiny RGB image type + PNG loader, used to CONSUME the platform `.scad`->skin chain's committed
//! gated assets (`skins/<dev>/body*.png`). Pure-Rust decode (the `png` crate) — no C, no GPU — so
//! the on-panel binary stays a single static aarch64-musl artifact.

use std::io;
use std::path::Path;

use crate::canvas::{rgb, Color};

/// A decoded RGB image (0x00RRGGBB per pixel, row-major).
#[derive(Clone)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<Color>,
}

impl Rgb {
    pub fn new(w: usize, h: usize, fill: Color) -> Rgb {
        Rgb { w, h, px: vec![fill; w * h] }
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> Color {
        self.px[y * self.w + x]
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, c: Color) {
        self.px[y * self.w + x] = c;
    }

    /// Copy the rectangle `(sx,sy,sw,sh)` from `src` onto `self` at the SAME position — the
    /// pf-hwprobe highlight compositing primitive (paste the active control's lit-atlas crop over
    /// the neutral body). `src` and `self` are the same dimensions (front/lit atlases are aligned).
    pub fn blit_region_from(&mut self, src: &Rgb, sx: i64, sy: i64, sw: i64, sh: i64) {
        for yy in sy..sy + sh {
            for xx in sx..sx + sw {
                if xx >= 0 && yy >= 0 && (xx as usize) < self.w.min(src.w) && (yy as usize) < self.h.min(src.h) {
                    let (x, y) = (xx as usize, yy as usize);
                    self.set(x, y, src.get(x, y));
                }
            }
        }
    }
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Decode a PNG file into an [`Rgb`]. Handles 8-bit RGB / RGBA (alpha dropped) / grayscale — the
/// formats the skin chain emits (the a133 skin is 8-bit RGB).
pub fn load_png(path: &Path) -> io::Result<Rgb> {
    let file = std::fs::File::open(path)?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().map_err(to_io)?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(to_io)?;
    let data = &buf[..info.buffer_size()];
    let (w, h) = (info.width as usize, info.height as usize);

    if info.bit_depth != png::BitDepth::Eight {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported PNG bit depth {:?} for {}", info.bit_depth, path.display()),
        ));
    }

    let mut px = Vec::with_capacity(w * h);
    match info.color_type {
        png::ColorType::Rgb => {
            for c in data.chunks_exact(3) {
                px.push(rgb(c[0], c[1], c[2]));
            }
        }
        png::ColorType::Rgba => {
            for c in data.chunks_exact(4) {
                px.push(rgb(c[0], c[1], c[2])); // alpha dropped (skin atlases are opaque)
            }
        }
        png::ColorType::Grayscale => {
            for &g in data {
                px.push(rgb(g, g, g));
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for c in data.chunks_exact(2) {
                px.push(rgb(c[0], c[0], c[0]));
            }
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported PNG color type {other:?} for {}", path.display()),
            ));
        }
    }
    if px.len() != w * h {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decoded {} px, expected {}x{} for {}", px.len(), w, h, path.display()),
        ));
    }
    Ok(Rgb { w, h, px })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_region_copies_the_rect_only() {
        let mut dst = Rgb::new(4, 4, rgb(0, 0, 0));
        let mut src = Rgb::new(4, 4, rgb(0, 0, 0));
        src.set(1, 1, rgb(255, 0, 0));
        src.set(2, 2, rgb(255, 0, 0));
        dst.blit_region_from(&src, 1, 1, 2, 2);
        assert_eq!(dst.get(1, 1), rgb(255, 0, 0));
        assert_eq!(dst.get(2, 2), rgb(255, 0, 0));
        assert_eq!(dst.get(0, 0), rgb(0, 0, 0)); // outside the rect untouched
        assert_eq!(dst.get(3, 3), rgb(0, 0, 0));
    }
}
