//! The `/dev/fb0` sink: mmap the framebuffer, read its geometry + pixel format via the fbdev
//! ioctls, and blit a logical 1280x720 [`Canvas`] onto the panel, letterboxed to the real
//! resolution. libc-only — no SDL, no GPU, no PowerVR blob — so this runs on a minimally
//! brought-up device and sidesteps tsp-osr entirely. The pure format/letterbox math is factored
//! out of the syscall path so it is unit-tested off-device (the mmap/ioctl part runs on-panel).

use std::io;
use std::os::unix::io::RawFd;

use crate::canvas::{channels, Canvas, Color};

// Arch-independent fbdev ioctl request numbers (fixed constants in <linux/fb.h>).
const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOGET_FSCREENINFO: libc::Ioctl = 0x4602;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct FbFixScreeninfo {
    id: [libc::c_char; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// The framebuffer's geometry + how to pack an (r,g,b) into one of its pixels.
#[derive(Clone, Copy, Debug)]
pub struct FbFormat {
    pub w: u32,
    pub h: u32,
    pub bpp: u32,
    pub line_length: u32,
    pub r_off: u32,
    pub r_len: u32,
    pub g_off: u32,
    pub g_len: u32,
    pub b_off: u32,
    pub b_len: u32,
}

impl FbFormat {
    /// Pack an sRGB `Color` into this framebuffer's native pixel word.
    pub fn pack(&self, c: Color) -> u32 {
        let (r, g, b) = channels(c);
        let scale = |v: u8, len: u32| -> u32 {
            if len == 0 {
                0
            } else {
                let max = (1u32 << len) - 1;
                (v as u32 * max + 127) / 255
            }
        };
        (scale(r, self.r_len) << self.r_off)
            | (scale(g, self.g_len) << self.g_off)
            | (scale(b, self.b_len) << self.b_off)
    }

    pub fn bytes_per_pixel(&self) -> usize {
        self.bpp.div_ceil(8) as usize
    }
}

/// The integer letterbox of a `src_w`x`src_h` image into a `dst_w`x`dst_h` frame, preserving
/// aspect: the largest integer-friendly fit, centered. Returns (draw_w, draw_h, off_x, off_y).
pub fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, 0, 0);
    }
    // draw_w/draw_h = src scaled by min(dst_w/src_w, dst_h/src_h), done in integer ratios.
    let draw_w = (dst_h as u64 * src_w as u64 / src_h as u64).min(dst_w as u64) as u32;
    let draw_h = (dst_w as u64 * src_h as u64 / src_w as u64).min(dst_h as u64) as u32;
    // Use whichever axis is the binding constraint.
    let (draw_w, draw_h) = if draw_w * src_h <= draw_h * src_w {
        (draw_w, (draw_w as u64 * src_h as u64 / src_w as u64) as u32)
    } else {
        ((draw_h as u64 * src_w as u64 / src_h as u64) as u32, draw_h)
    };
    let off_x = (dst_w.saturating_sub(draw_w)) / 2;
    let off_y = (dst_h.saturating_sub(draw_h)) / 2;
    (draw_w, draw_h, off_x, off_y)
}

/// A mmap'd framebuffer ready to receive frames.
pub struct FbDev {
    fd: RawFd,
    map: *mut u8,
    map_len: usize,
    fmt: FbFormat,
}

impl FbDev {
    pub fn open(path: &str) -> io::Result<FbDev> {
        let cpath = std::ffi::CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut var: FbVarScreeninfo = FbVarScreeninfo::default();
        let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var as *mut _) == 0
                && libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix as *mut _) == 0
        };
        if !ok {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let fmt = FbFormat {
            w: var.xres,
            h: var.yres,
            bpp: var.bits_per_pixel,
            line_length: fix.line_length,
            r_off: var.red.offset,
            r_len: var.red.length,
            g_off: var.green.offset,
            g_len: var.green.length,
            b_off: var.blue.offset,
            b_len: var.blue.length,
        };
        let map_len = fix.line_length as usize * var.yres as usize;
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        };
        if map == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(FbDev { fd, map: map as *mut u8, map_len, fmt })
    }

    pub fn format(&self) -> FbFormat {
        self.fmt
    }

    /// Blit a logical `CANVAS_W`x`CANVAS_H` canvas onto the panel, letterboxed + nearest-neighbor
    /// scaled, black borders. Cheap enough at the wizard's low frame rate (one full sweep/frame).
    pub fn present(&mut self, canvas: &Canvas) {
        let (draw_w, draw_h, off_x, off_y) = letterbox(canvas.w as u32, canvas.h as u32, self.fmt.w, self.fmt.h);
        let bpp = self.fmt.bytes_per_pixel();
        let ll = self.fmt.line_length as usize;
        let src = canvas.pixels();
        for dy in 0..self.fmt.h {
            let row = unsafe { self.map.add(dy as usize * ll) };
            for dx in 0..self.fmt.w {
                let word = if dx >= off_x && dx < off_x + draw_w && dy >= off_y && dy < off_y + draw_h {
                    let sx = ((dx - off_x) as u64 * canvas.w as u64 / draw_w as u64) as usize;
                    let sy = ((dy - off_y) as u64 * canvas.h as u64 / draw_h as u64) as usize;
                    let sx = sx.min(canvas.w - 1);
                    let sy = sy.min(canvas.h - 1);
                    self.fmt.pack(src[sy * canvas.w + sx])
                } else {
                    0 // black border
                };
                unsafe {
                    let p = row.add(dx as usize * bpp);
                    for b in 0..bpp {
                        *p.add(b) = ((word >> (b * 8)) & 0xff) as u8;
                    }
                }
            }
        }
    }
}

impl Drop for FbDev {
    fn drop(&mut self) {
        unsafe {
            if !self.map.is_null() {
                libc::munmap(self.map as *mut libc::c_void, self.map_len);
            }
            if self.fd >= 0 {
                libc::close(self.fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::rgb;

    #[test]
    fn pack_xrgb8888() {
        // 32bpp XRGB: R@16 G@8 B@0, each 8 bits.
        let f = FbFormat { w: 1, h: 1, bpp: 32, line_length: 4, r_off: 16, r_len: 8, g_off: 8, g_len: 8, b_off: 0, b_len: 8 };
        assert_eq!(f.pack(rgb(0xAB, 0xCD, 0xEF)), 0x00AB_CDEF);
        assert_eq!(f.bytes_per_pixel(), 4);
    }

    #[test]
    fn pack_rgb565() {
        // 16bpp RGB565: R@11 len5, G@5 len6, B@0 len5.
        let f = FbFormat { w: 1, h: 1, bpp: 16, line_length: 2, r_off: 11, r_len: 5, g_off: 5, g_len: 6, b_off: 0, b_len: 5 };
        // white -> all bits set
        assert_eq!(f.pack(rgb(255, 255, 255)), 0xFFFF);
        // pure red -> top 5 bits
        assert_eq!(f.pack(rgb(255, 0, 0)), 0xF800);
        assert_eq!(f.bytes_per_pixel(), 2);
    }

    #[test]
    fn letterbox_16x9_into_16x9_is_full() {
        let (w, h, ox, oy) = letterbox(1280, 720, 1280, 720);
        assert_eq!((w, h, ox, oy), (1280, 720, 0, 0));
    }

    #[test]
    fn letterbox_16x9_into_4x3_pillarless_topbottom() {
        // 1280x720 into 640x480: fit by width (640), height 360, centered vertically.
        let (w, h, ox, oy) = letterbox(1280, 720, 640, 480);
        assert_eq!(w, 640);
        assert_eq!(h, 360);
        assert_eq!(ox, 0);
        assert_eq!(oy, 60);
    }

    #[test]
    fn letterbox_into_taller_frame_centers_horizontally() {
        // 1280x720 into 720x1280 (portrait panel): fit by width (720) -> height 405, centered.
        let (w, h, ox, oy) = letterbox(1280, 720, 720, 1280);
        assert_eq!(w, 720);
        assert_eq!(h, 405);
        assert_eq!(ox, 0);
        assert_eq!(oy, (1280 - 405) / 2);
    }
}
