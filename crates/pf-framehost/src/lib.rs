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
///
/// The quarter turns count the same way as fbcon's `FB_ROTATE_*`: `Rotate90`
/// puts scene pixel (u, v) exactly where fbcon's CW blitter puts console cell
/// (column u, row v), and `Rotate270` where its CCW blitter does. The
/// connector "panel orientation" property is mapped with the kernel's own
/// meaning (see `panel_orientation_rotation`).
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
            read_fbcon_rotation(
                Path::new("/sys/class/graphics/fbcon/rotate"),
                Path::new("/sys/class/vtconsole"),
            )
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
        // Last resort when the kernel says nothing: a guess from the buffer's
        // shape, not a reading of any orientation source.
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

// fbcon rotations, uapi linux/fb.h FB_ROTATE_*.
const FB_ROTATE_UR: u32 = 0;
const FB_ROTATE_CW: u32 = 1;
const FB_ROTATE_UD: u32 = 2;
const FB_ROTATE_CCW: u32 = 3;

/// The kernel's meaning of the connector "panel orientation" property, as the
/// fbcon rotation its own fbdev client derives from it: drm_client_rotation
/// (drm_client_modeset.c) maps LEFT_UP to DRM_MODE_ROTATE_90 and RIGHT_UP to
/// DRM_MODE_ROTATE_270, counter-clockwise (uapi drm_mode.h), and
/// drm_setup_crtcs_fb (drm_fb_helper.c) turns those into FB_ROTATE_CCW and
/// FB_ROTATE_CW. LEFT_UP means the panel's left side is at the top of the
/// casing (drm_connector.h), so upright content has its top along the panel's
/// native left column, which is where fbcon's CCW blitter draws console row 0.
fn panel_orientation_fbcon_rotate(name: &str) -> Option<u32> {
    match name {
        "Normal" => Some(FB_ROTATE_UR),
        "Upside Down" => Some(FB_ROTATE_UD),
        "Left Side Up" => Some(FB_ROTATE_CCW),
        "Right Side Up" => Some(FB_ROTATE_CW),
        _ => None,
    }
}

/// fbcon rotation to presentation rotation. The only such table in this
/// crate: the connector property goes through it too, so the two sources
/// cannot disagree about the same panel.
fn fbcon_present_rotation(value: u32) -> Option<PresentRotation> {
    match value {
        FB_ROTATE_UR => Some(PresentRotation::Rotate0),
        FB_ROTATE_CW => Some(PresentRotation::Rotate90),
        FB_ROTATE_UD => Some(PresentRotation::Rotate180),
        FB_ROTATE_CCW => Some(PresentRotation::Rotate270),
        _ => None,
    }
}

fn panel_orientation_rotation(name: &str) -> Option<PresentRotation> {
    panel_orientation_fbcon_rotate(name).and_then(fbcon_present_rotation)
}

/// vtconsole name of fbcon (fbcon.c registers "frame buffer device").
const FBCON_VTCON_NAME: &str = "frame buffer device";
/// MAX_NR_CON_DRIVER: vtcon0 ..= vtcon15.
const VTCON_MAX: usize = 16;

/// fbcon's rotation, which the kernel derives from the same connector
/// property. fbcon's rotate_show reports 0 whenever fbcon drives no
/// framebuffer, which is a default and not a reading, so the value counts
/// only while fbcon's vtconsole is bound. The boot animator unbinds it before
/// painting, and after that the file would say 0.
fn read_fbcon_rotation(
    rotate: &Path,
    vtconsole_root: &Path,
) -> io::Result<Option<PresentRotation>> {
    if !fbcon_bound(vtconsole_root)? {
        return Ok(None);
    }
    let value = std::fs::read_to_string(rotate)?;
    Ok(value
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(fbcon_present_rotation))
}

/// Find fbcon's vtconsole by name, not by index.
fn fbcon_bound(vtconsole_root: &Path) -> io::Result<bool> {
    for index in 0..VTCON_MAX {
        let vtcon = vtconsole_root.join(format!("vtcon{index}"));
        let Ok(name) = std::fs::read_to_string(vtcon.join("name")) else {
            continue;
        };
        if name.contains(FBCON_VTCON_NAME) {
            return Ok(std::fs::read_to_string(vtcon.join("bind"))?.starts_with('1'));
        }
    }
    Ok(false)
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

/// uapi `struct drm_mode_modeinfo`; only ever a scratch target.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeInfo {
    clock: u32,
    timings: [u16; 10],
    vrefresh: u32,
    flags: u32,
    kind: u32,
    name: [libc::c_char; 32],
}

/// uapi DRM_MODE_PROP_ENUM.
const DRM_MODE_PROP_ENUM: u32 = 1 << 3;

const DRM_IOCTL_MODE_GETRESOURCES: libc::Ioctl = 0xc040_64a0_u32 as libc::Ioctl;
const DRM_IOCTL_MODE_GETCONNECTOR: libc::Ioctl = 0xc050_64a7_u32 as libc::Ioctl;
const DRM_IOCTL_MODE_GETPROPERTY: libc::Ioctl = 0xc040_64aa_u32 as libc::Ioctl;

/// The three read-only KMS queries the orientation lookup makes. `KmsFd` is
/// the real device; the tests substitute a device with the kernel's copy
/// rules.
trait KmsDevice {
    fn get_resources(&mut self, request: &mut DrmResources) -> io::Result<()>;
    fn get_connector(&mut self, request: &mut DrmConnector) -> io::Result<()>;
    fn get_property(&mut self, request: &mut DrmProperty) -> io::Result<()>;
}

struct KmsFd(RawFd);

impl KmsDevice for KmsFd {
    fn get_resources(&mut self, request: &mut DrmResources) -> io::Result<()> {
        drm_ioctl(self.0, DRM_IOCTL_MODE_GETRESOURCES, request)
    }
    fn get_connector(&mut self, request: &mut DrmConnector) -> io::Result<()> {
        drm_ioctl(self.0, DRM_IOCTL_MODE_GETCONNECTOR, request)
    }
    fn get_property(&mut self, request: &mut DrmProperty) -> io::Result<()> {
        drm_ioctl(self.0, DRM_IOCTL_MODE_GETPROPERTY, request)
    }
}

fn drm_ioctl<T>(fd: RawFd, request: libc::Ioctl, argument: &mut T) -> io::Result<()> {
    loop {
        if unsafe { libc::ioctl(fd, request, argument as *mut T) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
            return Err(error);
        }
    }
}

fn drm_panel_orientation(fbdev: &Path) -> io::Result<Option<PresentRotation>> {
    drm_panel_orientation_from(
        fbdev,
        Path::new("/sys/class/graphics"),
        Path::new("/dev/dri"),
        |path| {
            let file = File::open(path)?;
            drm_panel_orientation_device(&mut KmsFd(file.as_raw_fd()))
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

/// `connection` values, enum drm_connector_status (drm_connector.h).
const DRM_MODE_CONNECTED: u32 = 1;
const DRM_MODE_UNKNOWNCONNECTION: u32 = 3;

/// The "panel orientation" of the connectors the kernel's own fbdev client
/// enables: the connected ones, or the status-unknown ones when none is
/// connected (drm_client_connectors_enabled). The image boot animator reads
/// it the same way, so both painters agree on the handoff.
///
/// Every request names a buffer for each array whose count it passes and
/// passes zero for every other count. The kernel copies into any array whose
/// count asks for it and does not check for NULL first
/// (drm_mode_getresources, drm_mode_getconnector, drm_mode_getproperty_ioctl).
/// So re-sending a sizing call's counts without buffers is EFAULT, and it
/// hides the property behind a fallback.
fn drm_panel_orientation_device(
    device: &mut impl KmsDevice,
) -> io::Result<Option<PresentRotation>> {
    let mut sizing = DrmResources::default();
    device.get_resources(&mut sizing)?;
    let mut connector_ids = vec![0_u32; sizing.count_connectors as usize];
    let mut request = DrmResources {
        connector_id_ptr: connector_ids.as_mut_ptr() as u64,
        count_connectors: sizing.count_connectors,
        ..DrmResources::default()
    };
    device.get_resources(&mut request)?;
    connector_ids.truncate(request.count_connectors as usize);
    let mut connectors = Vec::with_capacity(connector_ids.len());
    for connector_id in connector_ids {
        connectors.push(drm_connector_orientation(device, connector_id)?);
    }
    Ok(enabled_connector_orientation(&connectors))
}

/// (connection status, orientation) of each connector -> the orientation of
/// the enabled ones, or `None` when they carry none or disagree.
fn enabled_connector_orientation(
    connectors: &[(u32, Option<PresentRotation>)],
) -> Option<PresentRotation> {
    let enabled = if connectors
        .iter()
        .any(|&(status, _)| status == DRM_MODE_CONNECTED)
    {
        DRM_MODE_CONNECTED
    } else {
        DRM_MODE_UNKNOWNCONNECTION
    };
    let mut found = None;
    for &(status, rotation) in connectors {
        let Some(rotation) = rotation.filter(|_| status == enabled) else {
            continue;
        };
        if found.is_some_and(|seen| seen != rotation) {
            return None;
        }
        found = Some(rotation);
    }
    found
}

fn drm_connector_orientation(
    device: &mut impl KmsDevice,
    connector_id: u32,
) -> io::Result<(u32, Option<PresentRotation>)> {
    // count_modes = 1 with a scratch mode: a zero count asks for a forced
    // probe of the connector, which reading a property must not cause.
    let mut scratch_mode = DrmModeInfo::default();
    let mut sizing = DrmConnector {
        modes_ptr: std::ptr::addr_of_mut!(scratch_mode) as u64,
        count_modes: 1,
        connector_id,
        ..DrmConnector::default()
    };
    device.get_connector(&mut sizing)?;
    let mut property_ids = vec![0_u32; sizing.count_props as usize];
    let mut property_values = vec![0_u64; sizing.count_props as usize];
    let mut request = DrmConnector {
        modes_ptr: std::ptr::addr_of_mut!(scratch_mode) as u64,
        props_ptr: property_ids.as_mut_ptr() as u64,
        prop_values_ptr: property_values.as_mut_ptr() as u64,
        count_modes: 1,
        count_props: sizing.count_props,
        connector_id,
        ..DrmConnector::default()
    };
    device.get_connector(&mut request)?;
    let count = property_ids.len().min(request.count_props as usize);
    let mut rotation = None;
    for (&property_id, &value) in property_ids[..count].iter().zip(&property_values[..count]) {
        if let Some(found) = drm_property_orientation(device, property_id, value)? {
            rotation = Some(found);
        }
    }
    Ok((request.connection, rotation))
}

fn drm_property_orientation(
    device: &mut impl KmsDevice,
    property_id: u32,
    current_value: u64,
) -> io::Result<Option<PresentRotation>> {
    let mut sizing = DrmProperty {
        prop_id: property_id,
        ..DrmProperty::default()
    };
    device.get_property(&mut sizing)?;
    if c_name(&sizing.name) != "panel orientation" {
        return Ok(None);
    }
    if sizing.flags & DRM_MODE_PROP_ENUM == 0 {
        return Err(invalid_orientation(
            "\"panel orientation\" is not an enum".into(),
        ));
    }
    let blank = DrmPropertyEnum {
        value: 0,
        name: [0; 32],
    };
    let mut enums = vec![blank; sizing.count_enum_blobs as usize];
    let mut request = DrmProperty {
        enum_blob_ptr: enums.as_mut_ptr() as u64,
        prop_id: property_id,
        count_enum_blobs: sizing.count_enum_blobs,
        ..DrmProperty::default()
    };
    device.get_property(&mut request)?;
    enums.truncate(request.count_enum_blobs as usize);
    let name = enums
        .iter()
        .find(|entry| entry.value == current_value)
        .map(|entry| c_name(&entry.name))
        .ok_or_else(|| {
            invalid_orientation(format!(
                "panel orientation {current_value} has no enum name"
            ))
        })?;
    // An orientation this table does not know is not a reading to guess from.
    panel_orientation_rotation(&name)
        .map(Some)
        .ok_or_else(|| invalid_orientation(format!("unknown panel orientation {name:?}")))
}

fn invalid_orientation(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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
        // Scene origin placement with the kernel's meaning of the property
        // (every pixel is checked by the goldens test below).
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
                PresentRotation::Rotate270,
                (0, 1279),
                (720, 1280),
            ),
            (
                "Right Side Up",
                PresentRotation::Rotate90,
                (719, 0),
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
            let sysfs = FbconSysfs::new(value, &[DUMMY_VTCON, ("(S) frame buffer device", "1")]);
            assert_eq!(sysfs.read().unwrap(), Some(expected));
        }
    }

    const DUMMY_VTCON: (&str, &str) = ("(S) dummy device", "0");

    /// /sys/class/graphics/fbcon/rotate and /sys/class/vtconsole/vtcon<i>/{name,bind}.
    struct FbconSysfs {
        root: tempfile::TempDir,
    }

    impl FbconSysfs {
        fn new(rotate: &str, vtcons: &[(&str, &str)]) -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("rotate"), rotate).unwrap();
            for (index, (name, bind)) in vtcons.iter().enumerate() {
                let vtcon = root.path().join(format!("vtconsole/vtcon{index}"));
                std::fs::create_dir_all(&vtcon).unwrap();
                std::fs::write(vtcon.join("name"), format!("{name}\n")).unwrap();
                std::fs::write(vtcon.join("bind"), format!("{bind}\n")).unwrap();
            }
            std::fs::create_dir_all(root.path().join("vtconsole")).unwrap();
            Self { root }
        }

        fn read(&self) -> io::Result<Option<PresentRotation>> {
            read_fbcon_rotation(
                &self.root.path().join("rotate"),
                &self.root.path().join("vtconsole"),
            )
        }
    }

    #[test]
    fn fbcon_rotate_counts_only_while_fbcon_is_bound() {
        let bound = FbconSysfs::new("3\n", &[DUMMY_VTCON, ("(M) frame buffer device", "1")]);
        assert_eq!(bound.read().unwrap(), Some(PresentRotation::Rotate270));
        // After the boot animator unbinds fbcon, rotate_show reports 0: a
        // default, not the panel. It must not become Rotate0.
        let unbound = FbconSysfs::new("0\n", &[DUMMY_VTCON, ("(M) frame buffer device", "0")]);
        assert_eq!(unbound.read().unwrap(), None);
        let absent = FbconSysfs::new("0\n", &[DUMMY_VTCON]);
        assert_eq!(absent.read().unwrap(), None);
        let unparsable = FbconSysfs::new("7\n", &[("(S) frame buffer device", "1")]);
        assert_eq!(unparsable.read().unwrap(), None);
        // an unbound fbcon falls through to geometry, exactly as no fbcon does
        assert_eq!(
            resolve_rotation(None, None, unbound.read().unwrap(), 720, 1280),
            resolve_rotation(None, None, None, 720, 1280)
        );
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

    // ---- orientation: the kernel's table, restated independently ----------
    //
    // connector "panel orientation" enum name (drm_connector.c:1241-1244)
    //   -> DRM_MODE_ROTATE_<deg>, counter-clockwise (drm_client_modeset.c:971-983,
    //      uapi drm_mode.h:159-163)
    //   -> fbcon rotate hint FB_ROTATE_UR=0 CW=1 UD=2 CCW=3
    //      (drm_fb_helper.c:1682-1702, uapi fb.h:235-238)
    // (kernel-sunxi-7.x@1b1da76f, device/a133). Nothing here is taken from the code
    // under test.
    const KERNEL_ORIENTATIONS: [(&str, u32, u32); 4] = [
        ("Normal", 0, 0),
        ("Upside Down", 180, 2),
        ("Left Side Up", 90, 3),
        ("Right Side Up", 270, 1),
    ];

    /// Where fbcon draws console cell (row, col) of a `fw` x `fh` font on a
    /// `vxres` x `vyres` buffer: the putcs image origin.
    ///   UR  bitblit.c:165-166     dx = col*fw,            dy = row*fh
    ///   CW  fbcon_cw.c:135-136    dx = vxres-(row+1)*fh,  dy = col*fw
    ///   UD  fbcon_ud.c:172-173    dx = vxres-(col+1)*fw,  dy = vyres-(row+1)*fh
    ///   CCW fbcon_ccw.c:150-151   dx = row*fh,            dy = vyres-(col+1)*fw
    fn fbcon_cell_origin(
        rotate: u32,
        (row, col): (usize, usize),
        (fw, fh): (usize, usize),
        (vxres, vyres): (usize, usize),
    ) -> (usize, usize) {
        match rotate {
            0 => (col * fw, row * fh),
            1 => (vxres - (row + 1) * fh, col * fw),
            2 => (vxres - (col + 1) * fw, vyres - (row + 1) * fh),
            3 => (row * fh, vyres - (col + 1) * fw),
            _ => unreachable!("fbcon rotate {rotate}"),
        }
    }

    /// The boot animator's scene and its coded card: every scene pixel carries
    /// its own (u, v) (image apps/pocketforge-boot-animator/tests/test_animator.py
    /// `card_pixel`), so any misplaced pixel is unambiguous.
    const SCENE: (u32, u32) = (1280, 720);

    fn card_frame() -> RasterFrame {
        let (width, height) = SCENE;
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for v in 0..height {
            for u in 0..width {
                rgba.extend_from_slice(&[
                    (u & 0xff) as u8,
                    (v & 0xff) as u8,
                    ((u >> 8) | ((v >> 8) << 3)) as u8,
                    0xff,
                ]);
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

    /// The animator's first-frame page for each orientation, generated from the
    /// real animator binary at image 476c1e15b88df34c963e5ed75d6d7f10a9633e17
    /// (apps/pocketforge-boot-animator/src/main.c sha256 2f85b28f8ef4ec7e...,
    /// unchanged since image#144) by
    /// `scripts/check-framehost-animator-orientation.py --image <checkout> --print`.
    /// Rows: property, xres, yres, line_length (the animator test's geometries,
    /// one of them padded), sha256 of the presented page.
    const ANIMATOR_FIRST_FRAME: [(&str, u32, u32, u32, &str); 4] = [
        (
            "Normal",
            1280,
            720,
            5120,
            "939f128416dd1fa884abfa9d15550ef31ff8e28676e6eb86249aef6558b5d87e",
        ),
        (
            "Upside Down",
            1280,
            720,
            5120,
            "f22c12ac37dadd2b80f77b73ce5ed40aeeb157bd715ddbdb2cd34bc80f5f97e5",
        ),
        (
            "Left Side Up",
            720,
            1280,
            3072,
            "4ee64e8df6bc4ffb26f68640b1b8284474c4886b9f42bf61d8d789c494584c77",
        ),
        (
            "Right Side Up",
            720,
            1280,
            2880,
            "33f1a584ae41a8d5eab7ba654b3b1e15c7e6e111e5091d3090e967c1ea5de927",
        ),
    ];

    /// Present the card through the production path for `property` onto an
    /// `xres` x `yres` buffer with `stride`; return the page bytes.
    fn present_card_for_property(property: &str, xres: u32, yres: u32, stride: u32) -> Vec<u8> {
        let rotation = panel_orientation_rotation(property)
            .unwrap_or_else(|| panic!("{property}: no rotation"));
        let info = FbInfo {
            width: xres,
            height: yres,
            virtual_height: yres,
            stride,
            format: PixelFormat::Xrgb8888,
            yoffset: 0,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(u64::from(stride * yres)).unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let mut host = FbdevHost::from_parts_with_rotation(
            file,
            info,
            Box::new(FakePan { calls, fail: false }),
            rotation,
            RotationSource::DrmPanelOrientation,
        )
        .unwrap();
        assert_eq!(
            (host.metrics().logical_width, host.metrics().logical_height),
            (SCENE.0 as f32, SCENE.1 as f32),
            "{property}: logical scene"
        );
        host.write_frame(&card_frame()).unwrap();
        let mut page = vec![0; stride as usize * yres as usize];
        host.file.seek(SeekFrom::Start(0)).unwrap();
        host.file.read_exact(&mut page).unwrap();
        page
    }

    #[test]
    fn orientation_goldens_place_every_pixel_where_fbcon_draws_it() {
        let card = card_frame();
        for (property, xres, yres, stride, _) in ANIMATOR_FIRST_FRAME {
            let (_, _, hint) = KERNEL_ORIENTATIONS
                .into_iter()
                .find(|(name, _, _)| *name == property)
                .unwrap();
            let page = present_card_for_property(property, xres, yres, stride);
            let mut misplaced = 0_usize;
            let mut first = None;
            for v in 0..SCENE.1 as usize {
                for u in 0..SCENE.0 as usize {
                    let (x, y) =
                        fbcon_cell_origin(hint, (v, u), (1, 1), (xres as usize, yres as usize));
                    let mut expected = [0; 4];
                    pack(PixelFormat::Xrgb8888, pixel(&card, u, v), &mut expected);
                    let offset = y * stride as usize + x * 4;
                    if page[offset..offset + 4] != expected {
                        misplaced += 1;
                        first.get_or_insert((u, v, x, y));
                    }
                }
            }
            assert_eq!(
                misplaced, 0,
                "{property}: {misplaced} scene pixels are not where fbcon (rotate {hint}) draws \
                 them; first scene (u, v) -> expected buffer (x, y): {first:?}"
            );
        }
    }

    #[test]
    fn animator_first_frame_pages_match_framehost() {
        for (property, xres, yres, stride, animator_sha256) in ANIMATOR_FIRST_FRAME {
            let page = present_card_for_property(property, xres, yres, stride);
            let digest: String = Sha256::digest(&page)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(
                digest, animator_sha256,
                "{property}: pf-framehost page differs from the boot animator's first frame"
            );
        }
    }

    fn fbcon_fixture_rotation(value: u32) -> Option<PresentRotation> {
        FbconSysfs::new(&format!("{value}\n"), &[("(S) frame buffer device", "1")])
            .read()
            .unwrap()
    }

    // ---- a KMS device with the kernel's copy rules --------------------------
    //
    // Each ioctl copies into a caller array whenever the caller's count asks
    // for it, with no NULL check before the copy:
    //   drm_mode_getresources   drm_mode_config.c:111-178 (copies while index < count)
    //   drm_mode_getconnector   drm_connector.c:3353-3365 encoders (count >= n && n),
    //                           :3374-3381 count_modes == 0 is a forced-probe request,
    //                           :3404-3444 modes (count >= n && n); properties via
    //                           drm_mode_object_get_properties (copies while index < count)
    //   drm_mode_getproperty    drm_property.c:480-510 values (index < count),
    //                           enums (count >= index + 1)
    // (kernel-sunxi-7.x@1b1da76f, device/a133). A copy to a NULL user pointer is EFAULT.

    struct FakeConnector {
        id: u32,
        status: u32,
        encoders: Vec<u32>,
        modes: usize,
        properties: Vec<(u32, u64)>,
    }

    struct FakeProperty {
        id: u32,
        name: &'static str,
        flags: u32,
        enums: Vec<(u64, &'static str)>,
    }

    #[derive(Default)]
    struct FakeKms {
        crtcs: Vec<u32>,
        encoders: Vec<u32>,
        connectors: Vec<FakeConnector>,
        properties: Vec<FakeProperty>,
        forced_probe_requests: usize,
    }

    fn put<T>(pointer: u64, index: usize, value: T) -> io::Result<()> {
        if pointer == 0 {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        // SAFETY: the kernel writes here too; a production request that names a
        // buffer shorter than its count is the memory bug these tests look for.
        unsafe { (pointer as *mut T).add(index).write_unaligned(value) };
        Ok(())
    }

    fn put_ids(pointer: u64, count: u32, ids: &[u32]) -> io::Result<u32> {
        for (index, &id) in ids.iter().enumerate() {
            if index < count as usize {
                put(pointer, index, id)?;
            }
        }
        Ok(ids.len() as u32)
    }

    fn c_array(text: &str) -> [libc::c_char; 32] {
        let mut out = [0; 32];
        for (slot, byte) in out.iter_mut().zip(text.bytes()) {
            *slot = byte as libc::c_char;
        }
        out
    }

    impl KmsDevice for FakeKms {
        fn get_resources(&mut self, request: &mut DrmResources) -> io::Result<()> {
            request.count_fbs = put_ids(request.fb_id_ptr, request.count_fbs, &[])?;
            request.count_crtcs = put_ids(request.crtc_id_ptr, request.count_crtcs, &self.crtcs)?;
            request.count_encoders = put_ids(
                request.encoder_id_ptr,
                request.count_encoders,
                &self.encoders,
            )?;
            let ids: Vec<u32> = self.connectors.iter().map(|c| c.id).collect();
            request.count_connectors =
                put_ids(request.connector_id_ptr, request.count_connectors, &ids)?;
            Ok(())
        }

        fn get_connector(&mut self, request: &mut DrmConnector) -> io::Result<()> {
            let connector = self
                .connectors
                .iter()
                .find(|c| c.id == request.connector_id)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            let encoders = connector.encoders.len();
            if request.count_encoders as usize >= encoders && encoders > 0 {
                for (index, &id) in connector.encoders.iter().enumerate() {
                    put(request.encoders_ptr, index, id)?;
                }
            }
            request.count_encoders = encoders as u32;
            if request.count_modes == 0 {
                self.forced_probe_requests += 1;
            }
            request.connection = connector.status;
            if request.count_modes as usize >= connector.modes && connector.modes > 0 {
                for index in 0..connector.modes {
                    put(request.modes_ptr, index, DrmModeInfo::default())?;
                }
            }
            request.count_modes = connector.modes as u32;
            for (index, &(id, value)) in connector.properties.iter().enumerate() {
                if index < request.count_props as usize {
                    put(request.props_ptr, index, id)?;
                    put(request.prop_values_ptr, index, value)?;
                }
            }
            request.count_props = connector.properties.len() as u32;
            Ok(())
        }

        fn get_property(&mut self, request: &mut DrmProperty) -> io::Result<()> {
            let property = self
                .properties
                .iter()
                .find(|p| p.id == request.prop_id)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            request.name = c_array(property.name);
            request.flags = property.flags;
            for (index, &(value, _)) in property.enums.iter().enumerate() {
                if index < request.count_values as usize {
                    put(request.values_ptr, index, value)?;
                }
            }
            request.count_values = property.enums.len() as u32;
            if property.flags & DRM_MODE_PROP_ENUM != 0 {
                for (index, &(value, name)) in property.enums.iter().enumerate() {
                    if request.count_enum_blobs as usize > index {
                        put(
                            request.enum_blob_ptr,
                            index,
                            DrmPropertyEnum {
                                value,
                                name: c_array(name),
                            },
                        )?;
                    }
                }
                request.count_enum_blobs = property.enums.len() as u32;
            }
            Ok(())
        }
    }

    const PANEL_ORIENTATION_ID: u32 = 54;

    /// The TSP as gpu-13 read it on device (build #14: card0 connector 53,
    /// "panel orientation" prop id 54): one CRTC, one DSI encoder, one
    /// connected connector with one mode, plus a DPMS property that is read and
    /// skipped.
    fn tsp_kms(status: u32, orientation: u64) -> FakeKms {
        FakeKms {
            crtcs: vec![41],
            encoders: vec![52],
            connectors: vec![FakeConnector {
                id: 53,
                status,
                encoders: vec![52],
                modes: 1,
                properties: vec![(2, 0), (PANEL_ORIENTATION_ID, orientation)],
            }],
            properties: vec![
                FakeProperty {
                    id: 2,
                    name: "DPMS",
                    flags: DRM_MODE_PROP_ENUM,
                    enums: vec![(0, "On"), (1, "Standby"), (2, "Suspend"), (3, "Off")],
                },
                FakeProperty {
                    id: PANEL_ORIENTATION_ID,
                    name: "panel orientation",
                    flags: DRM_MODE_PROP_ENUM,
                    enums: vec![
                        (0, "Normal"),
                        (1, "Upside Down"),
                        (2, "Left Side Up"),
                        (3, "Right Side Up"),
                    ],
                },
            ],
            forced_probe_requests: 0,
        }
    }

    #[test]
    fn kms_reads_give_the_kernel_a_buffer_for_every_copy_it_is_asked_for() {
        // KERNEL_ORIENTATIONS is in enum drm_panel_orientation order
        // (drm_connector.h:374-380), so its index is the property value.
        for (value, (property, _, hint)) in KERNEL_ORIENTATIONS.into_iter().enumerate() {
            let mut kms = tsp_kms(1, value as u64);
            let rotation = drm_panel_orientation_device(&mut kms);
            assert_eq!(
                rotation.as_ref().ok().copied().flatten(),
                fbcon_present_rotation(hint),
                "{property}: the property read must succeed on a real connector: {rotation:?}"
            );
            assert_eq!(
                kms.forced_probe_requests, 0,
                "{property}: a property read must not ask for a forced connector probe"
            );
        }
    }

    #[test]
    fn kms_orientation_comes_from_the_connectors_the_kernel_enables() {
        let left = Some(PresentRotation::Rotate270);
        let right = Some(PresentRotation::Rotate90);
        // connected connectors win; a disconnected one's value is ignored
        assert_eq!(
            enabled_connector_orientation(&[(2, right), (DRM_MODE_CONNECTED, left)]),
            left
        );
        // none connected: the status-unknown ones count
        assert_eq!(
            enabled_connector_orientation(&[(2, right), (DRM_MODE_UNKNOWNCONNECTION, left)]),
            left
        );
        // enabled connectors that disagree give no answer
        assert_eq!(
            enabled_connector_orientation(&[
                (DRM_MODE_CONNECTED, left),
                (DRM_MODE_CONNECTED, right)
            ]),
            None
        );
        assert_eq!(
            enabled_connector_orientation(&[
                (DRM_MODE_CONNECTED, None),
                (DRM_MODE_CONNECTED, left)
            ]),
            left
        );
        // a disconnected-only panel: no reading
        let mut kms = tsp_kms(2, 2);
        assert_eq!(drm_panel_orientation_device(&mut kms).unwrap(), None);
        // a connected connector with no "panel orientation" property: no reading
        let mut kms = tsp_kms(DRM_MODE_CONNECTED, 2);
        kms.connectors[0].properties.pop();
        assert_eq!(drm_panel_orientation_device(&mut kms).unwrap(), None);
        // a value the table does not know is an error, never a guess
        let mut kms = tsp_kms(DRM_MODE_CONNECTED, 4);
        kms.properties[1].enums.push((4, "Future Orientation"));
        assert_eq!(
            drm_panel_orientation_device(&mut kms).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn kms_structs_match_the_ioctl_encodings() {
        // _IOWR('d', nr, size): the size is bits 16..30 of the request number.
        for (request, size) in [
            (
                DRM_IOCTL_MODE_GETRESOURCES,
                std::mem::size_of::<DrmResources>(),
            ),
            (
                DRM_IOCTL_MODE_GETCONNECTOR,
                std::mem::size_of::<DrmConnector>(),
            ),
            (
                DRM_IOCTL_MODE_GETPROPERTY,
                std::mem::size_of::<DrmProperty>(),
            ),
        ] {
            assert_eq!((request as u32 >> 16) & 0x3fff, size as u32);
        }
        // uapi struct drm_mode_modeinfo and drm_mode_property_enum
        assert_eq!(std::mem::size_of::<DrmModeInfo>(), 68);
        assert_eq!(std::mem::size_of::<DrmPropertyEnum>(), 40);
    }

    #[test]
    fn drm_and_fbcon_sources_agree_for_every_orientation() {
        for (property, _, hint) in KERNEL_ORIENTATIONS {
            assert_eq!(
                panel_orientation_rotation(property),
                fbcon_fixture_rotation(hint),
                "{property}: the connector property and the fbcon hint the kernel derives from \
                 it (rotate={hint}) resolve to different presentations"
            );
        }
    }
}
