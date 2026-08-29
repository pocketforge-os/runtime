//! Frame hosts own presentation while `pf-render` owns pixels and layout.

use pf_ports::{FrameHost, PresentAck, PresentFailure, PresentResult};
use pf_render::{DamageRect, RasterFrame, Rasterizer};
use pf_scene::{Insets, Orientation, Scene, SurfaceMetrics};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};

pub struct OffscreenHost {
    metrics: SurfaceMetrics,
    renderer: Rasterizer,
    frame: Option<RasterFrame>,
    sequence: u64,
}

impl OffscreenHost {
    pub fn new(metrics: SurfaceMetrics) -> Self {
        Self {
            metrics,
            renderer: Rasterizer::new(),
            frame: None,
            sequence: 0,
        }
    }
    pub fn frame(&self) -> Option<&RasterFrame> {
        self.frame.as_ref()
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        self.frame.as_ref().map(|f| f.rgba.as_slice())
    }
}

impl FrameHost for OffscreenHost {
    fn metrics(&self) -> SurfaceMetrics {
        self.metrics
    }
    fn present(&mut self, scene: &Scene) -> PresentResult {
        self.frame = Some(
            self.renderer
                .render(scene, self.metrics)
                .map_err(|e| PresentFailure::Backend(format!("render: {e:?}")))?,
        );
        self.sequence += 1;
        Ok(PresentAck {
            sequence: self.sequence,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Xrgb8888,
    Rgb565,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FbInfo {
    pub width: u32,
    pub height: u32,
    pub virtual_height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub yoffset: u32,
}

trait Pan: Send {
    fn pan(&mut self, fd: RawFd, yoffset: u32) -> io::Result<()>;
}
struct IoctlPan;
impl Pan for IoctlPan {
    fn pan(&mut self, fd: RawFd, yoffset: u32) -> io::Result<()> {
        ioctl_pan(fd, yoffset)
    }
}

/// Linux framebuffer host. The syscall discovery below is adapted from
/// `pf-collect-ui/src/fbdev.rs`; no dimensions are inherited from that UI.
pub struct FbdevHost {
    metrics: SurfaceMetrics,
    info: FbInfo,
    file: File,
    pan: Box<dyn Pan>,
    renderer: Rasterizer,
    page: u32,
    sequence: u64,
    last_frame: Option<RasterFrame>,
    pending_damage: [Option<DamageRect>; 2],
}

impl FbdevHost {
    pub fn open(path: &str) -> Result<Self, PresentFailure> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(backend)?;
        let info = query_info(file.as_raw_fd()).map_err(backend)?;
        Self::from_parts(file, info, Box::new(IoctlPan))
    }

    pub fn from_file(file: File, info: FbInfo) -> Result<Self, PresentFailure> {
        Self::from_parts(file, info, Box::new(IoctlPan))
    }

    fn from_parts(file: File, info: FbInfo, pan: Box<dyn Pan>) -> Result<Self, PresentFailure> {
        if info.width == 0
            || info.height == 0
            || info.stride < info.width * bytes_per_pixel(info.format) as u32
        {
            return Err(PresentFailure::Rejected);
        }
        let metrics = SurfaceMetrics {
            logical_width: info.width as f32,
            logical_height: info.height as f32,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: if info.width >= info.height {
                Orientation::Landscape
            } else {
                Orientation::Portrait
            },
        };
        let page = if info.virtual_height >= info.height * 2 {
            (info.yoffset / info.height).min(1)
        } else {
            0
        };
        Ok(Self {
            metrics,
            info,
            file,
            pan,
            renderer: Rasterizer::new(),
            page,
            sequence: 0,
            last_frame: None,
            pending_damage: [None, None],
        })
    }

    pub fn frame(&self) -> Option<&RasterFrame> {
        self.last_frame.as_ref()
    }

    fn write_frame(&mut self, frame: &RasterFrame) -> io::Result<()> {
        let pages = if self.info.virtual_height >= self.info.height * 2 {
            2
        } else {
            1
        };
        self.page = if pages == 2 { self.page ^ 1 } else { 0 };
        for pending in self.pending_damage.iter_mut().take(pages as usize) {
            *pending = union_damage(*pending, frame.damage);
        }
        let Some(damage) = self.pending_damage[self.page as usize].take() else {
            return self
                .pan
                .pan(self.file.as_raw_fd(), self.page * self.info.height);
        };
        let page_bytes = self.info.stride as u64 * self.info.height as u64;
        let bpp = bytes_per_pixel(self.info.format);
        let mut row = vec![0; damage.width as usize * bpp];
        for y in damage.y as usize..(damage.y + damage.height) as usize {
            for x in damage.x as usize..(damage.x + damage.width) as usize {
                let rgba = &frame.rgba[(y * self.info.width as usize + x) * 4..][..4];
                let row_x = (x - damage.x as usize) * bpp;
                pack(self.info.format, rgba, &mut row[row_x..row_x + bpp]);
            }
            let offset = page_bytes * self.page as u64
                + y as u64 * self.info.stride as u64
                + damage.x as u64 * bpp as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&row)?;
        }
        self.file.flush()?;
        self.pan
            .pan(self.file.as_raw_fd(), self.page * self.info.height)
    }
}

fn union_damage(a: Option<DamageRect>, b: Option<DamageRect>) -> Option<DamageRect> {
    match (a, b) {
        (None, value) | (value, None) => value,
        (Some(a), Some(b)) => {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            let right = (a.x + a.width).max(b.x + b.width);
            let bottom = (a.y + a.height).max(b.y + b.height);
            Some(DamageRect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            })
        }
    }
}

impl FrameHost for FbdevHost {
    fn metrics(&self) -> SurfaceMetrics {
        self.metrics
    }
    fn present(&mut self, scene: &Scene) -> PresentResult {
        let frame = self
            .renderer
            .render(scene, self.metrics)
            .map_err(|e| PresentFailure::Backend(format!("render: {e:?}")))?;
        self.write_frame(&frame).map_err(backend)?;
        self.last_frame = Some(frame);
        self.sequence += 1;
        Ok(PresentAck {
            sequence: self.sequence,
        })
    }
}

fn backend(error: io::Error) -> PresentFailure {
    PresentFailure::Backend(error.to_string())
}
fn bytes_per_pixel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Xrgb8888 => 4,
        PixelFormat::Rgb565 => 2,
    }
}

fn pack(format: PixelFormat, rgba: &[u8], out: &mut [u8]) {
    match format {
        PixelFormat::Xrgb8888 => out.copy_from_slice(&[rgba[2], rgba[1], rgba[0], 0xff]),
        PixelFormat::Rgb565 => {
            let word = ((rgba[0] as u16 >> 3) << 11)
                | ((rgba[1] as u16 >> 2) << 5)
                | (rgba[2] as u16 >> 3);
            out.copy_from_slice(&word.to_le_bytes());
        }
    }
}

const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOGET_FSCREENINFO: libc::Ioctl = 0x4602;
const FBIOPAN_DISPLAY: libc::Ioctl = 0x4606;
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Var {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
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
struct Fix {
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
fn query_info(fd: RawFd) -> io::Result<FbInfo> {
    let mut var = Var::default();
    let mut fix: Fix = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) } != 0
        || unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let format = match (
        var.bits_per_pixel,
        var.red.offset,
        var.red.length,
        var.green.offset,
        var.green.length,
        var.blue.offset,
        var.blue.length,
    ) {
        (32, 16, 8, 8, 8, 0, 8) => PixelFormat::Xrgb8888,
        (16, 11, 5, 5, 6, 0, 5) => PixelFormat::Rgb565,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported framebuffer pixel format",
            ))
        }
    };
    Ok(FbInfo {
        width: var.xres,
        height: var.yres,
        virtual_height: var.yres_virtual,
        stride: fix.line_length,
        format,
        yoffset: var.yoffset,
    })
}
fn ioctl_pan(fd: RawFd, yoffset: u32) -> io::Result<()> {
    let mut var = Var::default();
    if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) } != 0 {
        return Err(io::Error::last_os_error());
    }
    var.xoffset = 0;
    var.yoffset = yoffset;
    if unsafe { libc::ioctl(fd, FBIOPAN_DISPLAY, &mut var) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_scene::{Bounds, Node, NodeAction, NodeId, Role};
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    fn scene() -> Scene {
        let n = Node::new(
            NodeId::new("card").unwrap(),
            Role::Button,
            "続ける",
            Bounds::new(7.0, 9.0, 120.0, 51.0),
            "card",
        )
        .with_action(NodeAction::Activate);
        Scene::new(n, NodeId::new("card").unwrap()).unwrap()
    }
    fn info(format: PixelFormat) -> FbInfo {
        FbInfo {
            width: 319,
            height: 181,
            virtual_height: 362,
            stride: 319 * bytes_per_pixel(format) as u32 + 8,
            format,
            yoffset: 0,
        }
    }
    struct FakePan {
        calls: Arc<Mutex<Vec<u32>>>,
        fail: bool,
    }
    impl Pan for FakePan {
        fn pan(&mut self, _: RawFd, y: u32) -> io::Result<()> {
            self.calls.lock().unwrap().push(y);
            if self.fail {
                Err(io::Error::new(io::ErrorKind::Other, "pan failed"))
            } else {
                Ok(())
            }
        }
    }
    fn host(format: PixelFormat, fail: bool) -> (FbdevHost, Arc<Mutex<Vec<u32>>>) {
        let file = tempfile::tempfile().unwrap();
        file.set_len(info(format).stride as u64 * info(format).virtual_height as u64)
            .unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let host = FbdevHost::from_parts(
            file,
            info(format),
            Box::new(FakePan {
                calls: calls.clone(),
                fail,
            }),
        )
        .unwrap();
        (host, calls)
    }
    fn overlapping_scene(order: [&str; 2]) -> Scene {
        let children = order.map(|id| {
            Node::new(
                NodeId::new(id).unwrap(),
                Role::Text,
                id,
                if id == "front" {
                    Bounds::new(20.0, 20.0, 80.0, 40.0)
                } else {
                    Bounds::new(50.0, 30.0, 80.0, 40.0)
                },
                "card",
            )
        });
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(0.0, 0.0, 150.0, 90.0),
            "root",
        )
        .with_action(NodeAction::Activate)
        .with_children(children.into());
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }
    fn wrapping_scene(label: &str) -> Scene {
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            label,
            Bounds::new(40.0, 30.0, 42.0, 24.0),
            "card",
        )
        .with_action(NodeAction::Activate);
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }
    fn assert_current_page_matches_frame(host: &mut FbdevHost) {
        let frame = host.last_frame.as_ref().unwrap();
        let bpp = bytes_per_pixel(host.info.format);
        let mut actual = vec![0; host.info.stride as usize * host.info.height as usize];
        let offset = host.info.stride as u64 * host.info.height as u64 * host.page as u64;
        host.file.seek(SeekFrom::Start(offset)).unwrap();
        host.file.read_exact(&mut actual).unwrap();
        for y in 0..host.info.height as usize {
            for x in 0..host.info.width as usize {
                let mut expected = [0; 4];
                let rgba = &frame.rgba[(y * host.info.width as usize + x) * 4..][..4];
                pack(host.info.format, rgba, &mut expected[..bpp]);
                let actual_offset = y * host.info.stride as usize + x * bpp;
                assert_eq!(
                    &actual[actual_offset..actual_offset + bpp],
                    &expected[..bpp],
                    "pixel mismatch at ({x}, {y})"
                );
            }
        }
    }
    #[test]
    fn offscreen_is_byte_identical_by_sha() {
        let m = SurfaceMetrics {
            logical_width: 319.0,
            logical_height: 181.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        };
        let mut a = OffscreenHost::new(m);
        let mut b = OffscreenHost::new(m);
        a.present(&scene()).unwrap();
        b.present(&scene()).unwrap();
        assert_eq!(
            Sha256::digest(a.bytes().unwrap()),
            Sha256::digest(b.bytes().unwrap())
        );
    }
    #[test]
    fn formats_stride_and_double_buffer_pan() {
        for format in [PixelFormat::Xrgb8888, PixelFormat::Rgb565] {
            let (mut h, calls) = host(format, false);
            h.present(&scene()).unwrap();
            h.present(&scene()).unwrap();
            assert_eq!(&*calls.lock().unwrap(), &[181, 0]);
            let len = h.file.metadata().unwrap().len();
            assert_eq!(len, h.info.stride as u64 * h.info.virtual_height as u64);
        }
    }
    #[test]
    fn pan_failure_is_typed() {
        let (mut h, _) = host(PixelFormat::Xrgb8888, true);
        assert!(
            matches!(h.present(&scene()),Err(PresentFailure::Backend(s)) if s.contains("pan failed"))
        );
    }
    #[test]
    fn hosts_agree_on_geometry_and_content() {
        let m = SurfaceMetrics {
            logical_width: 319.0,
            logical_height: 181.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        };
        let mut off = OffscreenHost::new(m);
        let (mut fb, _) = host(PixelFormat::Rgb565, false);
        off.present(&scene()).unwrap();
        fb.present(&scene()).unwrap();
        assert_eq!(off.metrics(), fb.metrics());
        assert_eq!(off.frame().unwrap().rgba, fb.frame().unwrap().rgba);
    }
    #[test]
    fn packing_is_exact() {
        let px = [0xab, 0xcd, 0xef, 0xff];
        let mut x = [0; 4];
        pack(PixelFormat::Xrgb8888, &px, &mut x);
        assert_eq!(x, [0xef, 0xcd, 0xab, 0xff]);
        let mut r = [0; 2];
        pack(PixelFormat::Rgb565, &[255, 255, 255, 255], &mut r);
        assert_eq!(r, [0xff, 0xff]);
    }

    #[test]
    fn sibling_order_change_repaints_fbdev_pixels() {
        let (mut host, _) = host(PixelFormat::Xrgb8888, false);
        let old = overlapping_scene(["front", "back"]);
        host.present(&old).unwrap();
        host.present(&old).unwrap();
        host.present(&overlapping_scene(["back", "front"])).unwrap();
        assert_eq!(
            host.frame().unwrap().damage,
            Some(DamageRect {
                x: 20,
                y: 20,
                width: 110,
                height: 50,
            })
        );
        assert_current_page_matches_frame(&mut host);
    }

    #[test]
    fn changed_wrapping_label_leaves_no_stale_fbdev_glyphs() {
        let (mut host, _) = host(PixelFormat::Xrgb8888, false);
        let old = wrapping_scene("This label wraps across far more lines than fit");
        host.present(&old).unwrap();
        host.present(&old).unwrap();
        host.present(&wrapping_scene("Short")).unwrap();
        assert_current_page_matches_frame(&mut host);
    }
}
