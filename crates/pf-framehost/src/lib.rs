//! Frame hosts own presentation while `pf-render` owns pixels and layout.

use pf_ports::{FrameHost, PresentAck, PresentFailure, PresentResult};
use pf_render::{DamageRect, RasterFrame, Rasterizer, RenderError, ThemeBase};
use pf_scene::{Insets, Orientation, Scene, SurfaceMetrics};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

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

    pub fn set_text_scale(&mut self, factor: f32) -> Result<(), RenderError> {
        self.renderer.set_text_scale(factor)
    }
}

impl FrameHost for OffscreenHost {
    fn metrics(&self) -> SurfaceMetrics {
        self.metrics
    }
    fn set_theme_base(&mut self, base: ThemeBase) {
        self.renderer.set_theme_base(base);
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

/// Clockwise rotation applied while copying the logical scene to the framebuffer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PresentRotation {
    #[default]
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl PresentRotation {
    pub fn from_degrees(value: &str) -> Option<Self> {
        match value {
            "0" => Some(Self::Rotate0),
            "90" => Some(Self::Rotate90),
            "180" => Some(Self::Rotate180),
            "270" => Some(Self::Rotate270),
            _ => None,
        }
    }

    pub fn degrees(self) -> u16 {
        match self {
            Self::Rotate0 => 0,
            Self::Rotate90 => 90,
            Self::Rotate180 => 180,
            Self::Rotate270 => 270,
        }
    }

    fn swaps_axes(self) -> bool {
        matches!(self, Self::Rotate90 | Self::Rotate270)
    }
}

/// Input which selected the fbdev presentation rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotationSource {
    Flag,
    DrmPanelOrientation,
    Fbcon,
    Geometry,
}

impl RotationSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::DrmPanelOrientation => "drm-panel-orientation",
            Self::Fbcon => "fbcon",
            Self::Geometry => "geometry",
        }
    }
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
    rotation: PresentRotation,
    rotation_source: RotationSource,
}

impl FbdevHost {
    pub fn open(path: &str) -> Result<Self, PresentFailure> {
        Self::open_with_rotation(path, None)
    }

    pub fn open_with_rotation(
        path: &str,
        requested: Option<PresentRotation>,
    ) -> Result<Self, PresentFailure> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(backend)?;
        let info = query_info(file.as_raw_fd()).map_err(backend)?;
        let (rotation, source) = resolve_rotation(
            requested,
            drm_panel_orientation(Path::new(path)).ok().flatten(),
            read_fbcon_rotation(Path::new("/sys/class/graphics/fbcon/rotate"))
                .ok()
                .flatten(),
            info.width,
            info.height,
        );
        eprintln!(
            "fbdev {}x{} present={} source={}",
            info.width,
            info.height,
            rotation.degrees(),
            source.label()
        );
        Self::from_parts_with_rotation(file, info, Box::new(IoctlPan), rotation, source)
    }

    pub fn from_file(file: File, info: FbInfo) -> Result<Self, PresentFailure> {
        Self::from_parts(file, info, Box::new(IoctlPan))
    }

    fn from_parts(file: File, info: FbInfo, pan: Box<dyn Pan>) -> Result<Self, PresentFailure> {
        Self::from_parts_with_rotation(
            file,
            info,
            pan,
            PresentRotation::Rotate0,
            RotationSource::Flag,
        )
    }

    fn from_parts_with_rotation(
        file: File,
        info: FbInfo,
        pan: Box<dyn Pan>,
        rotation: PresentRotation,
        rotation_source: RotationSource,
    ) -> Result<Self, PresentFailure> {
        if info.width == 0
            || info.height == 0
            || info.stride < info.width * bytes_per_pixel(info.format) as u32
        {
            return Err(PresentFailure::Rejected);
        }
        let (logical_width, logical_height) = if rotation.swaps_axes() {
            (info.height, info.width)
        } else {
            (info.width, info.height)
        };
        let metrics = SurfaceMetrics {
            logical_width: logical_width as f32,
            logical_height: logical_height as f32,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: if logical_width >= logical_height {
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
            rotation,
            rotation_source,
        })
    }

    pub fn frame(&self) -> Option<&RasterFrame> {
        self.last_frame.as_ref()
    }

    pub fn set_text_scale(&mut self, factor: f32) -> Result<(), RenderError> {
        self.renderer.set_text_scale(factor)
    }

    pub fn presentation_rotation(&self) -> (PresentRotation, RotationSource) {
        (self.rotation, self.rotation_source)
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
        // Rotated logical damage is not a contiguous framebuffer row range, so
        // copy the complete changed frame. On the target this is 3.7 MB/present.
        let _ = damage;
        let mut row = vec![0; self.info.width as usize * bpp];
        for y in 0..self.info.height as usize {
            for x in 0..self.info.width as usize {
                let (u, v) = source_coordinates(
                    self.rotation,
                    x,
                    y,
                    frame.width as usize,
                    frame.height as usize,
                );
                let rgba = &frame.rgba[(v * frame.width as usize + u) * 4..][..4];
                let row_x = x * bpp;
                pack(self.info.format, rgba, &mut row[row_x..row_x + bpp]);
            }
            let offset = page_bytes * self.page as u64 + y as u64 * self.info.stride as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&row)?;
        }
        self.file.flush()?;
        self.pan
            .pan(self.file.as_raw_fd(), self.page * self.info.height)
    }
}

fn source_coordinates(
    rotation: PresentRotation,
    x: usize,
    y: usize,
    scene_width: usize,
    scene_height: usize,
) -> (usize, usize) {
    match rotation {
        PresentRotation::Rotate0 => (x, y),
        PresentRotation::Rotate90 => (y, scene_height - 1 - x),
        PresentRotation::Rotate180 => (scene_width - 1 - x, scene_height - 1 - y),
        PresentRotation::Rotate270 => (scene_width - 1 - y, x),
    }
}

fn resolve_rotation(
    flag: Option<PresentRotation>,
    panel_orientation: Option<PresentRotation>,
    fbcon: Option<PresentRotation>,
    width: u32,
    height: u32,
) -> (PresentRotation, RotationSource) {
    if let Some(rotation) = flag {
        (rotation, RotationSource::Flag)
    } else if let Some(rotation) = panel_orientation {
        (rotation, RotationSource::DrmPanelOrientation)
    } else if let Some(rotation) = fbcon {
        (rotation, RotationSource::Fbcon)
    } else {
        (
            if height > width {
                PresentRotation::Rotate90
            } else {
                PresentRotation::Rotate0
            },
            RotationSource::Geometry,
        )
    }
}

fn panel_orientation_rotation(name: &str) -> Option<PresentRotation> {
    match name {
        "Normal" => Some(PresentRotation::Rotate0),
        "Left Side Up" => Some(PresentRotation::Rotate90),
        "Upside Down" => Some(PresentRotation::Rotate180),
        "Right Side Up" => Some(PresentRotation::Rotate270),
        _ => None,
    }
}

fn read_fbcon_rotation(path: &Path) -> io::Result<Option<PresentRotation>> {
    let value = std::fs::read_to_string(path)?;
    Ok(match value.trim() {
        "0" => Some(PresentRotation::Rotate0),
        "1" => Some(PresentRotation::Rotate90),
        "2" => Some(PresentRotation::Rotate180),
        "3" => Some(PresentRotation::Rotate270),
        _ => None,
    })
}

#[repr(C)]
#[derive(Default)]
struct DrmResources {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [libc::c_char; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmPropertyEnum {
    value: u64,
    name: [libc::c_char; 32],
}

const DRM_IOCTL_MODE_GETRESOURCES: libc::Ioctl = 0xc040_64a0_u32 as libc::Ioctl;
const DRM_IOCTL_MODE_GETCONNECTOR: libc::Ioctl = 0xc050_64a7_u32 as libc::Ioctl;
const DRM_IOCTL_MODE_GETPROPERTY: libc::Ioctl = 0xc040_64aa_u32 as libc::Ioctl;

fn drm_panel_orientation(fbdev: &Path) -> io::Result<Option<PresentRotation>> {
    drm_panel_orientation_from(
        fbdev,
        Path::new("/sys/class/graphics"),
        Path::new("/dev/dri"),
        |path| {
            let file = File::open(path)?;
            drm_panel_orientation_fd(file.as_raw_fd())
        },
    )
}

fn drm_panel_orientation_from<F>(
    fbdev: &Path,
    graphics_root: &Path,
    dri_root: &Path,
    mut read_card: F,
) -> io::Result<Option<PresentRotation>>
where
    F: FnMut(&Path) -> io::Result<Option<PresentRotation>>,
{
    let fb_name = fbdev
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fbdev has no basename"))?;
    let mut cards = Vec::new();
    let backing_dir = graphics_root.join(fb_name).join("device/drm");
    if let Ok(entries) = std::fs::read_dir(backing_dir) {
        cards.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .filter(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("card") && !name.contains('-')
                }),
        );
    }
    if cards.is_empty() {
        cards.push("card0".into());
    }

    for card in cards {
        if let Ok(Some(rotation)) = read_card(&dri_root.join(card)) {
            return Ok(Some(rotation));
        }
    }
    Ok(None)
}

fn drm_panel_orientation_fd(fd: RawFd) -> io::Result<Option<PresentRotation>> {
    let mut resources = DrmResources::default();
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut resources) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut connectors = vec![0_u32; resources.count_connectors as usize];
    resources.connector_id_ptr = connectors.as_mut_ptr() as u64;
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut resources) } != 0 {
        return Err(io::Error::last_os_error());
    }
    connectors.truncate(resources.count_connectors as usize);
    for connector_id in connectors {
        if let Some(rotation) = drm_connector_orientation(fd, connector_id)? {
            return Ok(Some(rotation));
        }
    }
    Ok(None)
}

fn drm_connector_orientation(fd: RawFd, connector_id: u32) -> io::Result<Option<PresentRotation>> {
    let mut connector = DrmConnector {
        connector_id,
        ..DrmConnector::default()
    };
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut connector) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if connector.connection != 1 || connector.count_props == 0 {
        return Ok(None);
    }
    let mut property_ids = vec![0_u32; connector.count_props as usize];
    let mut property_values = vec![0_u64; connector.count_props as usize];
    connector.props_ptr = property_ids.as_mut_ptr() as u64;
    connector.prop_values_ptr = property_values.as_mut_ptr() as u64;
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut connector) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let count = connector.count_props as usize;
    property_ids.truncate(count);
    property_values.truncate(count);
    for (&property_id, &value) in property_ids.iter().zip(&property_values) {
        if let Some(rotation) = drm_property_orientation(fd, property_id, value)? {
            return Ok(Some(rotation));
        }
    }
    Ok(None)
}

fn drm_property_orientation(
    fd: RawFd,
    property_id: u32,
    current_value: u64,
) -> io::Result<Option<PresentRotation>> {
    let mut property = DrmProperty {
        prop_id: property_id,
        ..DrmProperty::default()
    };
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &mut property) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if c_name(&property.name) != "panel orientation" {
        return Ok(None);
    }
    let blank = DrmPropertyEnum {
        value: 0,
        name: [0; 32],
    };
    let mut enums = vec![blank; property.count_enum_blobs as usize];
    property.enum_blob_ptr = enums.as_mut_ptr() as u64;
    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &mut property) } != 0 {
        return Err(io::Error::last_os_error());
    }
    enums.truncate(property.count_enum_blobs as usize);
    Ok(enums
        .iter()
        .find(|entry| entry.value == current_value)
        .and_then(|entry| panel_orientation_rotation(&c_name(&entry.name))))
}

fn c_name(bytes: &[libc::c_char]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|&byte| byte as u8 as char)
        .collect()
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
    fn set_theme_base(&mut self, base: ThemeBase) {
        self.renderer.set_theme_base(base);
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
            Role::Text,
            "続ける",
            Bounds::new(7.0, 9.0, 120.0, 51.0),
            "--state-rest-surface",
        );
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
                Err(io::Error::other("pan failed"))
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

    fn assert_high_contrast_frame(frame: &RasterFrame) {
        assert_eq!(&frame.rgba[..4], &[0, 0, 0, 255]);
        assert!(frame
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));
    }

    #[test]
    fn offscreen_theme_base_takes_effect_on_next_present() {
        let mut host = OffscreenHost::new(SurfaceMetrics {
            logical_width: 319.0,
            logical_height: 181.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        });
        host.set_theme_base(ThemeBase::HighContrast);
        host.present(&scene()).unwrap();
        assert_high_contrast_frame(host.frame().unwrap());
    }

    #[test]
    fn fbdev_theme_base_takes_effect_on_next_present() {
        let (mut host, _) = host(PixelFormat::Xrgb8888, false);
        host.set_theme_base(ThemeBase::HighContrast);
        host.present(&scene()).unwrap();
        assert_high_contrast_frame(host.frame().unwrap());
    }

    #[test]
    fn offscreen_text_scale_is_forwarded_and_validated() {
        let metrics = SurfaceMetrics {
            logical_width: 319.0,
            logical_height: 181.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        };
        let mut normal = OffscreenHost::new(metrics);
        normal.present(&scene()).unwrap();
        let mut large = OffscreenHost::new(metrics);
        large.set_text_scale(2.0).unwrap();
        large.present(&scene()).unwrap();

        assert_ne!(normal.bytes(), large.bytes());
        assert!(matches!(
            large.set_text_scale(f32::NAN),
            Err(RenderError::InvalidTextScale)
        ));
    }

    #[test]
    fn fbdev_text_scale_is_forwarded_and_validated() {
        let (mut normal, _) = host(PixelFormat::Xrgb8888, false);
        normal.present(&scene()).unwrap();
        let (mut large, _) = host(PixelFormat::Xrgb8888, false);
        large.set_text_scale(2.0).unwrap();
        large.present(&scene()).unwrap();

        assert_ne!(normal.frame().unwrap().rgba, large.frame().unwrap().rgba);
        assert!(matches!(
            large.set_text_scale(0.0),
            Err(RenderError::InvalidTextScale)
        ));
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
                "--state-rest-surface",
            )
        });
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(0.0, 0.0, 150.0, 90.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(children.into());
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }
    fn wrapping_scene(label: &str) -> Scene {
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Text,
            label,
            Bounds::new(40.0, 30.0, 42.0, 24.0),
            "--state-rest-surface",
        );
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

    fn pixel_frame(width: u32, height: u32) -> RasterFrame {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for v in 0..height {
            for u in 0..width {
                rgba.extend_from_slice(&[(u + 1) as u8, (v + 1) as u8, (u + v + 1) as u8, 255]);
            }
        }
        RasterFrame {
            width,
            height,
            rgba,
            damage: Some(DamageRect {
                x: 0,
                y: 0,
                width,
                height,
            }),
            notes: vec![],
        }
    }

    fn pixel(frame: &RasterFrame, u: usize, v: usize) -> &[u8] {
        &frame.rgba[(v * frame.width as usize + u) * 4..][..4]
    }

    #[test]
    fn clockwise_present_maps_landscape_scene_into_portrait_buffer() {
        let info = FbInfo {
            width: 720,
            height: 1280,
            virtual_height: 1280,
            stride: 720 * 4,
            format: PixelFormat::Xrgb8888,
            yoffset: 0,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(u64::from(info.stride * info.virtual_height))
            .unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let mut host = FbdevHost::from_parts_with_rotation(
            file,
            info,
            Box::new(FakePan { calls, fail: false }),
            PresentRotation::Rotate90,
            RotationSource::DrmPanelOrientation,
        )
        .unwrap();
        assert_eq!(host.metrics().logical_width, 1280.0);
        assert_eq!(host.metrics().logical_height, 720.0);
        assert_eq!(
            host.presentation_rotation(),
            (
                PresentRotation::Rotate90,
                RotationSource::DrmPanelOrientation
            )
        );
        let frame = pixel_frame(1280, 720);
        host.write_frame(&frame).unwrap();
        let mut bytes = vec![0; info.stride as usize * info.height as usize];
        host.file.seek(SeekFrom::Start(0)).unwrap();
        host.file.read_exact(&mut bytes).unwrap();
        for ((u, v), (x, y)) in [
            ((0, 0), (719, 0)),
            ((1279, 719), (0, 1279)),
            ((835, 417), (302, 835)),
        ] {
            let mut expected = [0; 4];
            pack(PixelFormat::Xrgb8888, pixel(&frame, u, v), &mut expected);
            let offset = y * info.stride as usize + x * 4;
            assert_eq!(&bytes[offset..offset + 4], &expected, "scene ({u},{v})");
        }
    }

    #[test]
    fn unrotated_present_is_byte_identical_to_row_packing() {
        let info = FbInfo {
            width: 1280,
            height: 720,
            virtual_height: 720,
            stride: 1280 * 4,
            format: PixelFormat::Xrgb8888,
            yoffset: 0,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(u64::from(info.stride * info.virtual_height))
            .unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let mut host = FbdevHost::from_parts_with_rotation(
            file,
            info,
            Box::new(FakePan { calls, fail: false }),
            PresentRotation::Rotate0,
            RotationSource::Flag,
        )
        .unwrap();
        let frame = pixel_frame(1280, 720);
        host.write_frame(&frame).unwrap();
        let mut actual = vec![0; info.stride as usize * info.height as usize];
        host.file.seek(SeekFrom::Start(0)).unwrap();
        host.file.read_exact(&mut actual).unwrap();
        let mut expected = Vec::with_capacity(actual.len());
        for rgba in frame.rgba.chunks_exact(4) {
            let mut packed = [0; 4];
            pack(PixelFormat::Xrgb8888, rgba, &mut packed);
            expected.extend_from_slice(&packed);
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn rotation_resolver_honors_priority_and_geometry() {
        assert_eq!(
            resolve_rotation(
                Some(PresentRotation::Rotate180),
                Some(PresentRotation::Rotate270),
                Some(PresentRotation::Rotate90),
                720,
                1280,
            ),
            (PresentRotation::Rotate180, RotationSource::Flag)
        );
        assert_eq!(
            resolve_rotation(
                None,
                Some(PresentRotation::Rotate270),
                Some(PresentRotation::Rotate90),
                720,
                1280,
            ),
            (
                PresentRotation::Rotate270,
                RotationSource::DrmPanelOrientation
            )
        );
        assert_eq!(
            resolve_rotation(None, None, Some(PresentRotation::Rotate180), 720, 1280),
            (PresentRotation::Rotate180, RotationSource::Fbcon)
        );
        assert_eq!(
            resolve_rotation(None, None, None, 720, 1280),
            (PresentRotation::Rotate90, RotationSource::Geometry)
        );
        assert_eq!(
            resolve_rotation(None, None, None, 1280, 720),
            (PresentRotation::Rotate0, RotationSource::Geometry)
        );
    }

    #[test]
    fn orientation_fixtures_map_to_rotations() {
        for (name, expected, buffer_position, buffer_size) in [
            ("Normal", PresentRotation::Rotate0, (0, 0), (1280, 720)),
            (
                "Upside Down",
                PresentRotation::Rotate180,
                (1279, 719),
                (1280, 720),
            ),
            (
                "Left Side Up",
                PresentRotation::Rotate90,
                (719, 0),
                (720, 1280),
            ),
            (
                "Right Side Up",
                PresentRotation::Rotate270,
                (0, 1279),
                (720, 1280),
            ),
        ] {
            assert_eq!(panel_orientation_rotation(name), Some(expected), "{name}");
            let (x, y) = buffer_position;
            let (buffer_width, buffer_height) = buffer_size;
            assert!(x < buffer_width && y < buffer_height, "{name}");
            assert_eq!(
                source_coordinates(expected, x, y, 1280, 720),
                (0, 0),
                "scene origin placement for {name}"
            );
        }
        assert_eq!(panel_orientation_rotation("Bottom Up"), None);

        for (value, expected) in [
            ("0\n", PresentRotation::Rotate0),
            ("1\n", PresentRotation::Rotate90),
            ("2\n", PresentRotation::Rotate180),
            ("3\n", PresentRotation::Rotate270),
        ] {
            let mut fixture = tempfile::NamedTempFile::new().unwrap();
            fixture.write_all(value.as_bytes()).unwrap();
            assert_eq!(read_fbcon_rotation(fixture.path()).unwrap(), Some(expected));
        }
    }

    #[test]
    fn drm_discovery_uses_card0_when_fb_sysfs_has_no_card_link() {
        let fixture = tempfile::tempdir().unwrap();
        let graphics = fixture.path().join("graphics");
        let dri = fixture.path().join("dri");
        std::fs::create_dir_all(&graphics).unwrap();
        std::fs::create_dir_all(&dri).unwrap();
        let mut visited = Vec::new();
        let rotation = drm_panel_orientation_from(Path::new("/dev/fb0"), &graphics, &dri, |path| {
            visited.push(path.to_path_buf());
            Ok(Some(PresentRotation::Rotate90))
        })
        .unwrap();
        assert_eq!(rotation, Some(PresentRotation::Rotate90));
        assert_eq!(visited, [dri.join("card0")]);
    }

    fn assert_discovered_card_does_not_fall_back_to_card0(
        card1_result: io::Result<Option<PresentRotation>>,
    ) {
        let fixture = tempfile::tempdir().unwrap();
        let graphics = fixture.path().join("graphics");
        let dri = fixture.path().join("dri");
        std::fs::create_dir_all(graphics.join("fb0/device/drm/card1")).unwrap();
        std::fs::create_dir_all(&dri).unwrap();
        let mut visited = Vec::new();
        let mut card1_result = Some(card1_result);
        let rotation = drm_panel_orientation_from(Path::new("/dev/fb0"), &graphics, &dri, |path| {
            visited.push(path.to_path_buf());
            card1_result.take().unwrap()
        })
        .unwrap();
        assert_eq!(rotation, None);
        assert_eq!(visited, [dri.join("card1")]);
    }

    #[test]
    fn discovered_card_without_orientation_does_not_consult_card0() {
        assert_discovered_card_does_not_fall_back_to_card0(Ok(None));
    }

    #[test]
    fn unreadable_discovered_card_does_not_consult_card0() {
        assert_discovered_card_does_not_fall_back_to_card0(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unreadable card",
        )));
    }

    fn assert_unknown_drm_result_falls_through(result: io::Result<Option<PresentRotation>>) {
        let fixture = tempfile::tempdir().unwrap();
        let graphics = fixture.path().join("graphics");
        let dri = fixture.path().join("dri");
        std::fs::create_dir_all(&graphics).unwrap();
        let mut result = Some(result);
        assert_eq!(
            drm_panel_orientation_from(Path::new("/dev/fb0"), &graphics, &dri, |_| {
                result.take().unwrap()
            })
            .unwrap(),
            None
        );
        assert_eq!(
            resolve_rotation(None, None, Some(PresentRotation::Rotate0), 720, 1280),
            (PresentRotation::Rotate0, RotationSource::Fbcon)
        );
    }

    #[test]
    fn missing_drm_card_falls_through() {
        assert_unknown_drm_result_falls_through(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "missing card",
        )));
    }

    #[test]
    fn missing_drm_connector_falls_through() {
        assert_unknown_drm_result_falls_through(Ok(None));
    }

    #[test]
    fn missing_drm_property_falls_through() {
        assert_unknown_drm_result_falls_through(Ok(None));
    }

    #[test]
    fn unknown_drm_property_value_falls_through() {
        assert_eq!(panel_orientation_rotation("Future Orientation"), None);
        assert_unknown_drm_result_falls_through(Ok(None));
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
